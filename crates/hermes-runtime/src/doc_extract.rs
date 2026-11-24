//! Pure-Rust document-to-text extraction for `read_file`.
//!
//! Mirrors `tools/read_extract.py`:
//! - `.ipynb`: Jupyter Notebook v4/v3 Markdown/Code/Raw cells + formatted outputs
//! - `.docx`: OOXML `word/document.xml` paragraphs, tabs, breaks (skipping ruby `<w:rt>`)
//! - `.xlsx`: OOXML visible sheets, shared strings, inline strings, booleans, base-26 cell refs
//! - `.pptx`: OOXML slide-ordered text extraction (`ppt/slides/slideN.xml`)
//! - `.db` / `.sqlite` / `.sqlite3`: read-only immutable SQLite schema + 5-row previews
//! - `.pdf`: `pdftotext` with scanned-page coverage warnings, plus pure-Rust DEFLATE text fallback
//!   and `[NEEDS OCR]` recovery hint for image-only PDFs

use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use rusqlite::{Connection, OpenFlags};
use serde_json::Value;

pub(crate) const MAX_DOCUMENT_BYTES: u64 = 50 * 1024 * 1024;
const MAX_XLSX_ROWS_PER_SHEET: usize = 5000;
const MAX_XLSX_COLS: usize = 256;
const MAX_NOTEBOOK_OUTPUT_CHARS: usize = 20_000;
const SQLITE_MAGIC: &[u8; 16] = b"SQLite format 3\x00";
const SQLITE_PREVIEW_ROWS: usize = 5;
const SQLITE_MAX_TABLES: usize = 200;
const SQLITE_CELL_CHARS: usize = 80;

const PDF_EMPTY_PAGE_CHARS: usize = 20;
const PDF_COVERAGE_MIN_EMPTY: usize = 2;
const PDF_COVERAGE_MIN_RATIO: f64 = 0.2;
const PDF_COVERAGE_ABSOLUTE_EMPTY: usize = 10;
const PDF_PAGE_SCAN_TIMEOUT: Duration = Duration::from_secs(20);
const PDF_GAP_MAP_MAX_ENTRIES: usize = 20;
const GAP_CONTEXT_CHARS: usize = 60;

pub(crate) const EXTRACTABLE_EXTENSIONS: &[&str] = &[
    ".ipynb", ".docx", ".xlsx", ".pptx", ".pdf", ".db", ".sqlite", ".sqlite3",
];

pub(crate) const BINARY_DOCUMENT_EXTENSIONS: &[&str] = &[
    ".docx", ".xlsx", ".pptx", ".pdf", ".db", ".sqlite", ".sqlite3",
];

pub(crate) fn lower_extension(path: &Path) -> String {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| format!(".{}", ext.to_ascii_lowercase()))
        .unwrap_or_default()
}

pub(crate) fn is_extractable_document(path: &Path) -> bool {
    let ext = lower_extension(path);
    EXTRACTABLE_EXTENSIONS.contains(&ext.as_str())
}

pub(crate) fn is_binary_document(path: &Path) -> bool {
    let ext = lower_extension(path);
    BINARY_DOCUMENT_EXTENSIONS.contains(&ext.as_str())
}

#[cfg_attr(windows, allow(dead_code))]
pub(crate) fn is_sqlite_sidecar(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let lower = name.to_ascii_lowercase();
    lower.ends_with("-wal") || lower.ends_with("-shm") || lower.ends_with("-journal")
}

pub(crate) fn extract_document_bytes(
    data: &[u8],
    resolved: &Path,
    display_path: &str,
) -> Result<String, String> {
    if data.len() as u64 > MAX_DOCUMENT_BYTES {
        return Err(format!(
            "Document too large to convert ({} bytes, limit is {} bytes)",
            data.len(),
            MAX_DOCUMENT_BYTES
        ));
    }
    match lower_extension(resolved).as_str() {
        ".ipynb" => extract_notebook(data, display_path),
        ".docx" => extract_docx(data),
        ".xlsx" => extract_xlsx(data),
        ".pptx" => extract_pptx(data),
        ".db" | ".sqlite" | ".sqlite3" => extract_sqlite(data),
        ".pdf" => extract_pdf(data, display_path),
        other => Err(format!("Unsupported document type: '{other}'")),
    }
}

fn joined_lines(lines: &[String], empty_error: &str) -> Result<String, String> {
    if !lines.iter().any(|line| !line.trim().is_empty()) {
        return Err(empty_error.to_owned());
    }
    let mut out = lines.join("\n");
    while out.ends_with('\n') {
        out.pop();
    }
    out.push('\n');
    Ok(out)
}

// ── RFC 1951 DEFLATE & Zlib ────────────────────────────────────────────────────────────────────

struct BitReader<'a> {
    data: &'a [u8],
    byte_pos: usize,
    bit_buf: u32,
    bit_cnt: u8,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            byte_pos: 0,
            bit_buf: 0,
            bit_cnt: 0,
        }
    }

    fn read_bit(&mut self) -> Result<u32, String> {
        if self.bit_cnt == 0 {
            let b = *self
                .data
                .get(self.byte_pos)
                .ok_or_else(|| "Unexpected end of DEFLATE stream".to_owned())?;
            self.byte_pos += 1;
            self.bit_buf = u32::from(b);
            self.bit_cnt = 8;
        }
        let bit = self.bit_buf & 1;
        self.bit_buf >>= 1;
        self.bit_cnt -= 1;
        Ok(bit)
    }

    fn read_bits(&mut self, n: u8) -> Result<u32, String> {
        while self.bit_cnt < n {
            let b = *self
                .data
                .get(self.byte_pos)
                .ok_or_else(|| "Unexpected end of DEFLATE stream".to_owned())?;
            self.byte_pos += 1;
            self.bit_buf |= u32::from(b) << self.bit_cnt;
            self.bit_cnt += 8;
        }
        let mask = if n == 32 { u32::MAX } else { (1u32 << n) - 1 };
        let val = self.bit_buf & mask;
        self.bit_buf >>= n;
        self.bit_cnt -= n;
        Ok(val)
    }

    fn align_to_byte(&mut self) {
        self.bit_buf = 0;
        self.bit_cnt = 0;
    }

    fn read_u16_le(&mut self) -> Result<u16, String> {
        self.align_to_byte();
        if self.byte_pos + 2 > self.data.len() {
            return Err("Unexpected end of stored DEFLATE block".to_owned());
        }
        let val = u16::from_le_bytes([self.data[self.byte_pos], self.data[self.byte_pos + 1]]);
        self.byte_pos += 2;
        Ok(val)
    }

    fn read_bytes(&mut self, len: usize) -> Result<&'a [u8], String> {
        self.align_to_byte();
        let end = self
            .byte_pos
            .checked_add(len)
            .ok_or_else(|| "DEFLATE block length overflow".to_owned())?;
        let slice = self
            .data
            .get(self.byte_pos..end)
            .ok_or_else(|| "Unexpected end of stored DEFLATE block".to_owned())?;
        self.byte_pos = end;
        Ok(slice)
    }
}

const HUFF_MAX_BITS: usize = 15;

struct HuffTable {
    counts: [u16; HUFF_MAX_BITS + 1],
    symbols: Vec<u16>,
}

impl HuffTable {
    fn from_lengths(lengths: &[u8]) -> Result<Self, String> {
        let mut counts = [0u16; HUFF_MAX_BITS + 1];
        for &len in lengths {
            let l = usize::from(len);
            if l > HUFF_MAX_BITS {
                return Err("Invalid Huffman code length".to_owned());
            }
            counts[l] += 1;
        }
        if counts[0] as usize == lengths.len() {
            return Ok(Self {
                counts,
                symbols: Vec::new(),
            });
        }
        let mut left = 1i32;
        for count in counts.iter().skip(1) {
            left <<= 1;
            left -= i32::from(*count);
            if left < 0 {
                return Err("Oversubscribed Huffman code table".to_owned());
            }
        }
        let mut offsets = [0usize; HUFF_MAX_BITS + 1];
        for len in 1..HUFF_MAX_BITS {
            offsets[len + 1] = offsets[len] + usize::from(counts[len]);
        }
        let total: usize = counts.iter().skip(1).map(|&c| usize::from(c)).sum();
        let mut symbols = vec![0u16; total];
        for (sym, &len) in lengths.iter().enumerate() {
            if len != 0 {
                let l = usize::from(len);
                symbols[offsets[l]] = sym as u16;
                offsets[l] += 1;
            }
        }
        Ok(Self { counts, symbols })
    }

    fn decode(&self, reader: &mut BitReader<'_>) -> Result<u16, String> {
        let mut code = 0i32;
        let mut first = 0i32;
        let mut index = 0usize;
        for len in 1..=HUFF_MAX_BITS {
            code |= reader.read_bit()? as i32;
            let count = i32::from(self.counts[len]);
            if code - count < first {
                let pos = index + (code - first) as usize;
                return self
                    .symbols
                    .get(pos)
                    .copied()
                    .ok_or_else(|| "Invalid Huffman symbol".to_owned());
            }
            index += count as usize;
            first = (first + count) << 1;
            code <<= 1;
        }
        Err("Invalid Huffman code in DEFLATE stream".to_owned())
    }
}

fn fixed_tables() -> (HuffTable, HuffTable) {
    let mut lit = [0u8; 288];
    for (i, slot) in lit.iter_mut().enumerate() {
        *slot = match i {
            0..=143 => 8,
            144..=255 => 9,
            256..=279 => 7,
            _ => 8,
        };
    }
    let dist = [5u8; 32];
    (
        HuffTable::from_lengths(&lit).expect("valid fixed literal table"),
        HuffTable::from_lengths(&dist).expect("valid fixed distance table"),
    )
}

fn dynamic_tables(reader: &mut BitReader<'_>) -> Result<(HuffTable, HuffTable), String> {
    const CL_ORDER: [usize; 19] = [
        16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
    ];
    let hlit = reader.read_bits(5)? as usize + 257;
    let hdist = reader.read_bits(5)? as usize + 1;
    let hclen = reader.read_bits(4)? as usize + 4;
    if hlit > 286 || hdist > 30 {
        return Err("Invalid dynamic Huffman header counts".to_owned());
    }
    let mut cl_lens = [0u8; 19];
    for &idx in CL_ORDER.iter().take(hclen) {
        cl_lens[idx] = reader.read_bits(3)? as u8;
    }
    let cl_table = HuffTable::from_lengths(&cl_lens)?;
    let total = hlit + hdist;
    let mut lengths = Vec::with_capacity(total);
    while lengths.len() < total {
        let sym = cl_table.decode(reader)?;
        match sym {
            0..=15 => lengths.push(sym as u8),
            16 => {
                let prev = *lengths
                    .last()
                    .ok_or_else(|| "Invalid code length repeat in DEFLATE".to_owned())?;
                let repeat = reader.read_bits(2)? as usize + 3;
                if lengths.len() + repeat > total {
                    return Err("Code length repeat overflows table".to_owned());
                }
                lengths.resize(lengths.len() + repeat, prev);
            }
            17 => {
                let repeat = reader.read_bits(3)? as usize + 3;
                if lengths.len() + repeat > total {
                    return Err("Code length zero-repeat overflows table".to_owned());
                }
                lengths.resize(lengths.len() + repeat, 0);
            }
            18 => {
                let repeat = reader.read_bits(7)? as usize + 11;
                if lengths.len() + repeat > total {
                    return Err("Code length long zero-repeat overflows table".to_owned());
                }
                lengths.resize(lengths.len() + repeat, 0);
            }
            _ => return Err("Invalid code-length symbol".to_owned()),
        }
    }
    let lit_table = HuffTable::from_lengths(&lengths[..hlit])?;
    let dist_table = HuffTable::from_lengths(&lengths[hlit..])?;
    Ok((lit_table, dist_table))
}

fn decode_huffman_block(
    reader: &mut BitReader<'_>,
    lit_table: &HuffTable,
    dist_table: &HuffTable,
    out: &mut Vec<u8>,
    max_out: usize,
) -> Result<(), String> {
    const LEN_BASE: [usize; 29] = [
        3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115,
        131, 163, 195, 227, 258,
    ];
    const LEN_EXTRA: [u8; 29] = [
        0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
    ];
    const DIST_BASE: [usize; 30] = [
        1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
        2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
    ];
    const DIST_EXTRA: [u8; 30] = [
        0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12,
        13, 13,
    ];

    loop {
        let sym = lit_table.decode(reader)?;
        match sym {
            0..=255 => {
                if out.len() >= max_out {
                    return Err("Decompressed output exceeds size limit".to_owned());
                }
                out.push(sym as u8);
            }
            256 => return Ok(()),
            257..=285 => {
                let idx = usize::from(sym - 257);
                let len = LEN_BASE[idx] + reader.read_bits(LEN_EXTRA[idx])? as usize;
                let dsym = dist_table.decode(reader)? as usize;
                if dsym >= DIST_BASE.len() {
                    return Err("Invalid distance symbol in DEFLATE stream".to_owned());
                }
                let dist = DIST_BASE[dsym] + reader.read_bits(DIST_EXTRA[dsym])? as usize;
                if dist > out.len() || dist == 0 {
                    return Err("Invalid back-reference distance in DEFLATE stream".to_owned());
                }
                if out.len() + len > max_out {
                    return Err("Decompressed output exceeds size limit".to_owned());
                }
                for _ in 0..len {
                    let b = out[out.len() - dist];
                    out.push(b);
                }
            }
            _ => return Err("Invalid literal/length symbol in DEFLATE stream".to_owned()),
        }
    }
}

pub(crate) fn inflate_raw(input: &[u8], max_out: usize) -> Result<Vec<u8>, String> {
    let mut reader = BitReader::new(input);
    let mut out = Vec::new();
    loop {
        let bfinal = reader.read_bit()?;
        let btype = reader.read_bits(2)?;
        match btype {
            0 => {
                let len = usize::from(reader.read_u16_le()?);
                let nlen = reader.read_u16_le()?;
                if (len as u16) ^ 0xffff != nlen {
                    return Err("Corrupt stored block length in DEFLATE stream".to_owned());
                }
                if out.len() + len > max_out {
                    return Err("Decompressed output exceeds size limit".to_owned());
                }
                let slice = reader.read_bytes(len)?;
                out.extend_from_slice(slice);
            }
            1 => {
                let (lit, dist) = fixed_tables();
                decode_huffman_block(&mut reader, &lit, &dist, &mut out, max_out)?;
            }
            2 => {
                let (lit, dist) = dynamic_tables(&mut reader)?;
                decode_huffman_block(&mut reader, &lit, &dist, &mut out, max_out)?;
            }
            _ => return Err("Reserved DEFLATE block type 3".to_owned()),
        }
        if bfinal != 0 {
            break;
        }
    }
    Ok(out)
}

pub(crate) fn inflate_zlib(input: &[u8], max_out: usize) -> Result<Vec<u8>, String> {
    if input.len() < 2 {
        return Err("Truncated zlib header".to_owned());
    }
    let cmf = input[0];
    let flg = input[1];
    if (cmf & 0x0f) != 8 {
        return Err("Unsupported zlib compression method".to_owned());
    }
    if (u16::from(cmf) * 256 + u16::from(flg)) % 31 != 0 {
        return Err("Invalid zlib header checksum".to_owned());
    }
    let mut offset = 2;
    if (flg & 0x20) != 0 {
        if input.len() < 6 {
            return Err("Truncated zlib preset dictionary".to_owned());
        }
        offset += 4;
    }
    inflate_raw(&input[offset..], max_out)
}

// ── Minimal ZIP Reader ─────────────────────────────────────────────────────────────────────────

struct ZipEntry {
    name: String,
    method: u16,
    compressed_size: usize,
    uncompressed_size: usize,
    local_header_offset: usize,
}

pub(crate) struct ZipArchive<'a> {
    data: &'a [u8],
    entries: HashMap<String, ZipEntry>,
    order: Vec<String>,
}

fn u16_le(slice: &[u8], at: usize) -> Result<u16, String> {
    let bytes = slice
        .get(at..at + 2)
        .ok_or_else(|| "Unexpected EOF in ZIP archive".to_owned())?;
    Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
}

fn u32_le(slice: &[u8], at: usize) -> Result<u32, String> {
    let bytes = slice
        .get(at..at + 4)
        .ok_or_else(|| "Unexpected EOF in ZIP archive".to_owned())?;
    Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

impl<'a> ZipArchive<'a> {
    pub(crate) fn from_bytes(data: &'a [u8], kind: &str) -> Result<Self, String> {
        if data.len() < 22 {
            return Err(format!("Not a valid {kind}: truncated ZIP archive"));
        }
        let search_start = data.len().saturating_sub(22 + 65_535);
        let mut eocd = None;
        for pos in (search_start..=data.len() - 22).rev() {
            if data[pos..pos + 4] == [0x50, 0x4b, 0x05, 0x06] {
                let comment_len = usize::from(u16_le(data, pos + 20)?);
                if pos + 22 + comment_len == data.len() {
                    eocd = Some(pos);
                    break;
                }
            }
        }
        let eocd =
            eocd.ok_or_else(|| format!("Not a valid {kind}: missing ZIP central directory"))?;
        let cd_count = usize::from(u16_le(data, eocd + 10)?);
        let cd_size = usize::try_from(u32_le(data, eocd + 12)?).unwrap_or(usize::MAX);
        let cd_offset = usize::try_from(u32_le(data, eocd + 16)?).unwrap_or(usize::MAX);
        if cd_offset.saturating_add(cd_size) > data.len() {
            return Err(format!(
                "Not a valid {kind}: central directory out of bounds"
            ));
        }

        let mut entries = HashMap::new();
        let mut order = Vec::new();
        let mut cursor = cd_offset;
        for _ in 0..cd_count {
            if cursor + 46 > data.len() || data[cursor..cursor + 4] != [0x50, 0x4b, 0x01, 0x02] {
                return Err(format!("Not a valid {kind}: malformed ZIP directory entry"));
            }
            let method = u16_le(data, cursor + 10)?;
            let compressed_size = usize::try_from(u32_le(data, cursor + 20)?).unwrap_or(usize::MAX);
            let uncompressed_size =
                usize::try_from(u32_le(data, cursor + 24)?).unwrap_or(usize::MAX);
            let name_len = usize::from(u16_le(data, cursor + 28)?);
            let extra_len = usize::from(u16_le(data, cursor + 30)?);
            let comment_len = usize::from(u16_le(data, cursor + 32)?);
            let local_header_offset =
                usize::try_from(u32_le(data, cursor + 42)?).unwrap_or(usize::MAX);
            let name_start = cursor + 46;
            let name_end = name_start + name_len;
            let name_bytes = data
                .get(name_start..name_end)
                .ok_or_else(|| format!("Not a valid {kind}: truncated ZIP filename"))?;
            let name = String::from_utf8_lossy(name_bytes).into_owned();
            cursor = name_end + extra_len + comment_len;
            order.push(name.clone());
            entries.insert(
                name.clone(),
                ZipEntry {
                    name,
                    method,
                    compressed_size,
                    uncompressed_size,
                    local_header_offset,
                },
            );
        }
        Ok(Self {
            data,
            entries,
            order,
        })
    }

    fn contains(&self, name: &str) -> bool {
        self.entries.contains_key(name)
    }

    pub(crate) fn entry_names(&self) -> &[String] {
        &self.order
    }

    pub(crate) fn read_entry(&self, name: &str) -> Result<Vec<u8>, String> {
        let entry = self
            .entries
            .get(name)
            .ok_or_else(|| format!("Missing {name}"))?;
        let off = entry.local_header_offset;
        if off + 30 > self.data.len() || self.data[off..off + 4] != [0x50, 0x4b, 0x03, 0x04] {
            return Err(format!("Malformed local ZIP header for {}", entry.name));
        }
        let local_name_len = usize::from(u16_le(self.data, off + 26)?);
        let local_extra_len = usize::from(u16_le(self.data, off + 28)?);
        let payload_start = off + 30 + local_name_len + local_extra_len;
        let payload_end = payload_start
            .checked_add(entry.compressed_size)
            .ok_or_else(|| format!("Size overflow reading {}", entry.name))?;
        let payload = self
            .data
            .get(payload_start..payload_end)
            .ok_or_else(|| format!("Truncated ZIP entry {}", entry.name))?;
        let max_out = MAX_DOCUMENT_BYTES as usize;
        if entry.uncompressed_size > max_out {
            return Err(format!(
                "ZIP entry {} exceeds max document size",
                entry.name
            ));
        }
        match entry.method {
            0 => Ok(payload.to_vec()),
            8 => inflate_raw(payload, max_out),
            other => Err(format!(
                "Unsupported ZIP compression method {other} for {}",
                entry.name
            )),
        }
    }
}

// ── Lightweight XML Parser ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
enum XmlNode {
    Element {
        name: String,
        attrs: Vec<(String, String)>,
        children: Vec<XmlNode>,
    },
    Text(String),
}

impl XmlNode {
    fn local_name(&self) -> &str {
        match self {
            XmlNode::Element { name, .. } => name.rsplit(':').next().unwrap_or(name),
            XmlNode::Text(_) => "",
        }
    }

    fn attr(&self, target: &str) -> Option<&str> {
        let XmlNode::Element { attrs, .. } = self else {
            return None;
        };
        attrs
            .iter()
            .find(|(k, _)| k == target || k.rsplit(':').next() == Some(target))
            .map(|(_, v)| v.as_str())
    }

    fn children(&self) -> &[XmlNode] {
        match self {
            XmlNode::Element { children, .. } => children,
            XmlNode::Text(_) => &[],
        }
    }

    fn collect_text(&self) -> String {
        let mut out = String::new();
        self.push_text_into(&mut out);
        out
    }

    fn push_text_into(&self, out: &mut String) {
        match self {
            XmlNode::Text(t) => out.push_str(t),
            XmlNode::Element { children, .. } => {
                for child in children {
                    child.push_text_into(out);
                }
            }
        }
    }
}

fn find_descendants<'a>(node: &'a XmlNode, local: &str, out: &mut Vec<&'a XmlNode>) {
    if let XmlNode::Element { children, .. } = node {
        if node.local_name() == local {
            out.push(node);
        }
        for child in children {
            find_descendants(child, local, out);
        }
    }
}

fn decode_xml_entities(raw: &str) -> String {
    if !raw.contains('&') {
        return raw.to_owned();
    }
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp + 1..];
        if let Some(semi) = after.find(';') {
            let entity = &after[..semi];
            let replacement = match entity {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                _ if entity.starts_with("#x") || entity.starts_with("#X") => {
                    u32::from_str_radix(&entity[2..], 16)
                        .ok()
                        .and_then(char::from_u32)
                }
                _ if entity.starts_with('#') => {
                    entity[1..].parse::<u32>().ok().and_then(char::from_u32)
                }
                _ => None,
            };
            if let Some(ch) = replacement {
                out.push(ch);
                rest = &after[semi + 1..];
                continue;
            }
        }
        out.push('&');
        rest = after;
    }
    out.push_str(rest);
    out
}

struct XmlFrame {
    name: String,
    attrs: Vec<(String, String)>,
    children: Vec<XmlNode>,
}

struct OpenTag {
    consumed: usize,
    self_closing: bool,
    name: String,
    attrs: Vec<(String, String)>,
}

fn parse_xml(bytes: &[u8], part: &str) -> Result<XmlNode, String> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| format!("Malformed XML in {part}: invalid UTF-8"))?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut stack: Vec<XmlFrame> = Vec::new();
    let mut root: Option<XmlNode> = None;
    let mut pos = 0usize;
    let bytes_str = text.as_bytes();

    while pos < bytes_str.len() {
        if let Some(rel_lt) = text[pos..].find('<') {
            if rel_lt > 0 {
                let raw_text = decode_xml_entities(&text[pos..pos + rel_lt]);
                if let Some(top) = stack.last_mut() {
                    top.children.push(XmlNode::Text(raw_text));
                }
                pos += rel_lt;
            }
            let after = &text[pos + 1..];
            if let Some(comment) = after.strip_prefix("!--") {
                let Some(end) = comment.find("-->") else {
                    return Err(format!("Malformed XML in {part}: unclosed comment"));
                };
                pos += 1 + 3 + end + 3;
            } else if let Some(cdata) = after.strip_prefix("![CDATA[") {
                let Some(end) = cdata.find("]]>") else {
                    return Err(format!("Malformed XML in {part}: unclosed CDATA"));
                };
                if let Some(top) = stack.last_mut() {
                    top.children.push(XmlNode::Text(cdata[..end].to_owned()));
                }
                pos += 1 + 8 + end + 3;
            } else if let Some(pi) = after.strip_prefix('?') {
                let Some(end) = pi.find("?>") else {
                    return Err(format!("Malformed XML in {part}: unclosed instruction"));
                };
                pos += 1 + 1 + end + 2;
            } else if let Some(decl) = after.strip_prefix('!') {
                let Some(end) = decl.find('>') else {
                    return Err(format!("Malformed XML in {part}: unclosed declaration"));
                };
                pos += 1 + 1 + end + 1;
            } else if let Some(close) = after.strip_prefix('/') {
                let Some(end) = close.find('>') else {
                    return Err(format!("Malformed XML in {part}: unclosed closing tag"));
                };
                let tag_name = close[..end].trim();
                let frame = stack
                    .pop()
                    .ok_or_else(|| format!("Malformed XML in {part}: unexpected </{tag_name}>"))?;
                if frame.name != tag_name {
                    return Err(format!(
                        "Malformed XML in {part}: mismatched </{tag_name}> for <{}>",
                        frame.name
                    ));
                }
                let elem = XmlNode::Element {
                    name: frame.name,
                    attrs: frame.attrs,
                    children: frame.children,
                };
                if let Some(parent) = stack.last_mut() {
                    parent.children.push(elem);
                } else {
                    root = Some(elem);
                }
                pos += 1 + 1 + end + 1;
            } else {
                let tag = parse_open_tag(after, part)?;
                pos += 1 + tag.consumed;
                if tag.self_closing {
                    let elem = XmlNode::Element {
                        name: tag.name,
                        attrs: tag.attrs,
                        children: Vec::new(),
                    };
                    if let Some(parent) = stack.last_mut() {
                        parent.children.push(elem);
                    } else {
                        root = Some(elem);
                    }
                } else {
                    stack.push(XmlFrame {
                        name: tag.name,
                        attrs: tag.attrs,
                        children: Vec::new(),
                    });
                }
            }
        } else {
            let tail = decode_xml_entities(&text[pos..]);
            if let Some(top) = stack.last_mut() {
                top.children.push(XmlNode::Text(tail));
            }
            break;
        }
    }
    if !stack.is_empty() {
        return Err(format!("Malformed XML in {part}: unclosed element(s)"));
    }
    root.ok_or_else(|| format!("Malformed XML in {part}: empty XML document"))
}

fn parse_open_tag(after_lt: &str, part: &str) -> Result<OpenTag, String> {
    let bytes = after_lt.as_bytes();
    let mut idx = 0usize;
    while idx < bytes.len() && bytes[idx].is_ascii_whitespace() {
        idx += 1;
    }
    let name_start = idx;
    while idx < bytes.len()
        && !bytes[idx].is_ascii_whitespace()
        && bytes[idx] != b'>'
        && bytes[idx] != b'/'
    {
        idx += 1;
    }
    let name = after_lt[name_start..idx].to_owned();
    if name.is_empty() {
        return Err(format!("Malformed XML in {part}: empty tag name"));
    }
    let mut attrs = Vec::new();
    let mut self_closing = false;
    loop {
        while idx < bytes.len() && bytes[idx].is_ascii_whitespace() {
            idx += 1;
        }
        if idx >= bytes.len() {
            return Err(format!("Malformed XML in {part}: unclosed <{name}>"));
        }
        if bytes[idx] == b'/' {
            idx += 1;
            while idx < bytes.len() && bytes[idx].is_ascii_whitespace() {
                idx += 1;
            }
            if idx >= bytes.len() || bytes[idx] != b'>' {
                return Err(format!("Malformed XML in {part}: expected '>' after '/'"));
            }
            self_closing = true;
            idx += 1;
            break;
        }
        if bytes[idx] == b'>' {
            idx += 1;
            break;
        }
        let k_start = idx;
        while idx < bytes.len()
            && !bytes[idx].is_ascii_whitespace()
            && bytes[idx] != b'='
            && bytes[idx] != b'>'
        {
            idx += 1;
        }
        let key = after_lt[k_start..idx].to_owned();
        while idx < bytes.len() && bytes[idx].is_ascii_whitespace() {
            idx += 1;
        }
        if idx >= bytes.len() || bytes[idx] != b'=' {
            return Err(format!(
                "Malformed XML in {part}: expected '=' after '{key}'"
            ));
        }
        idx += 1;
        while idx < bytes.len() && bytes[idx].is_ascii_whitespace() {
            idx += 1;
        }
        let quote = *bytes
            .get(idx)
            .filter(|&&b| b == b'"' || b == b'\'')
            .ok_or_else(|| format!("Malformed XML in {part}: expected quoted attribute"))?;
        idx += 1;
        let v_start = idx;
        while idx < bytes.len() && bytes[idx] != quote {
            idx += 1;
        }
        if idx >= bytes.len() {
            return Err(format!("Malformed XML in {part}: unclosed attribute quote"));
        }
        let val = decode_xml_entities(&after_lt[v_start..idx]);
        idx += 1;
        attrs.push((key, val));
    }
    Ok(OpenTag {
        consumed: idx,
        self_closing,
        name,
        attrs,
    })
}

// ── Jupyter Notebook (.ipynb) Extractor ────────────────────────────────────────────────────────

fn source_text(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

fn clean_stream_text(text: &str) -> String {
    let stripped = crate::session_search::strip_ansi(text).replace("\r\n", "\n");
    stripped
        .split('\n')
        .map(|line| {
            line.split('\r')
                .rfind(|part| !part.is_empty())
                .unwrap_or("")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn approx_base64_bytes(payload: &str) -> usize {
    let clean: String = payload
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '='))
        .collect();
    let pad = clean.chars().rev().take(2).filter(|&c| c == '=').count();
    ((clean.len() * 3) / 4).saturating_sub(pad)
}

fn human_size(bytes: usize) -> String {
    if bytes >= 1024 {
        format!("{} KB", (bytes as f64 / 1024.0).round() as usize)
    } else {
        format!("{bytes} B")
    }
}

fn notebook_output_text(output: &Value) -> String {
    let Some(obj) = output.as_object() else {
        return String::new();
    };
    let otype = obj.get("output_type").and_then(Value::as_str).unwrap_or("");
    if otype == "stream" {
        let body = clean_stream_text(&source_text(obj.get("text")));
        return if body.trim().is_empty() {
            String::new()
        } else {
            body
        };
    }
    if matches!(otype, "error" | "pyerr") {
        let tb = obj
            .get("traceback")
            .and_then(Value::as_array)
            .map(|arr| {
                clean_stream_text(
                    &arr.iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join("\n"),
                )
            })
            .unwrap_or_default();
        let ename = obj.get("ename").and_then(Value::as_str).unwrap_or("");
        let evalue = obj.get("evalue").and_then(Value::as_str).unwrap_or("");
        let header = format!("Error: {ename}: {evalue}")
            .trim_end_matches(": ")
            .to_owned();
        return format!("{header}\n{tb}").trim_end().to_owned();
    }
    if !matches!(otype, "execute_result" | "display_data" | "pyout") {
        return String::new();
    }
    let mut data_map: serde_json::Map<String, Value> =
        match obj.get("data").and_then(Value::as_object) {
            Some(map) => map.clone(),
            None => {
                let mut m = serde_json::Map::new();
                if obj.get("text").is_some() {
                    m.insert("text/plain".into(), obj["text"].clone());
                }
                for (k, mime) in [
                    ("png", "image/png"),
                    ("jpeg", "image/jpeg"),
                    ("svg", "image/svg+xml"),
                    ("html", "text/html"),
                ] {
                    if let Some(val) = obj.get(k) {
                        m.insert(mime.into(), val.clone());
                    }
                }
                m
            }
        };
    if data_map.contains_key("application/vnd.jupyter.widget-view+json") {
        return "[interactive widget — omitted]".to_owned();
    }
    for mime in ["text/plain", "text/markdown"] {
        if let Some(val) = data_map.get(mime) {
            let body = clean_stream_text(&source_text(Some(val)));
            if !body.trim().is_empty() {
                return body;
            }
        }
    }
    for (mime, val) in &data_map {
        if mime.starts_with("image/") {
            let bytes = approx_base64_bytes(&source_text(Some(val)));
            return format!("[{mime} output — {}, omitted]", human_size(bytes));
        }
    }
    if let Some(html) = data_map.remove("text/html") {
        let len = source_text(Some(&html)).chars().count();
        return format!("[text/html output — {len} chars, omitted]");
    }
    let keys: Vec<&str> = data_map.keys().map(String::as_str).collect();
    if keys.is_empty() {
        "[unknown output — omitted]".to_owned()
    } else {
        format!("[{} output — omitted]", keys.join(", "))
    }
}

fn shell_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

fn notebook_outputs(cell: &Value, jq_pointer: &str, filename: &str) -> String {
    let Some(outputs) = cell.get("outputs").and_then(Value::as_array) else {
        return String::new();
    };
    let joined = outputs
        .iter()
        .map(notebook_output_text)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    let count = joined.chars().count();
    if count <= MAX_NOTEBOOK_OUTPUT_CHARS {
        return joined;
    }
    let prefix: String = joined.chars().take(MAX_NOTEBOOK_OUTPUT_CHARS).collect();
    let omitted = count - MAX_NOTEBOOK_OUTPUT_CHARS;
    let hint = if !jq_pointer.is_empty() && !filename.is_empty() {
        format!(
            " — full output: jq -r '{jq_pointer}' {}",
            shell_single_quote(filename)
        )
    } else {
        String::new()
    };
    format!("{prefix}\n… [{omitted} output chars truncated{hint}]")
}

fn extract_notebook(data: &[u8], display_path: &str) -> Result<String, String> {
    let text = std::str::from_utf8(data).map_err(|_| "Not a valid notebook: invalid UTF-8")?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let nb: Value = serde_json::from_str(text).map_err(|e| format!("Not a valid notebook: {e}"))?;
    let Some(nb_obj) = nb.as_object() else {
        return Err("Notebook root is not an object".to_owned());
    };
    let mut cells: Vec<(String, &Value)> = Vec::new();
    if let Some(raw_cells) = nb_obj.get("cells").and_then(Value::as_array) {
        for (i, cell) in raw_cells.iter().enumerate() {
            cells.push((format!(".cells[{i}].outputs"), cell));
        }
    } else if let Some(worksheets) = nb_obj.get("worksheets").and_then(Value::as_array) {
        for (wi, ws) in worksheets.iter().enumerate() {
            if let Some(ws_cells) = ws.get("cells").and_then(Value::as_array) {
                for (ci, cell) in ws_cells.iter().enumerate() {
                    cells.push((format!(".worksheets[{wi}].cells[{ci}].outputs"), cell));
                }
            }
        }
    }
    if cells.is_empty() {
        return Err("Notebook contains no cells".to_owned());
    }
    let mut counts: HashMap<&str, usize> = HashMap::new();
    let mut out: Vec<String> = Vec::new();
    for (jq_ptr, cell) in cells {
        let Some(typ) = cell.get("cell_type").and_then(Value::as_str) else {
            continue;
        };
        let label = match typ {
            "markdown" => "Markdown",
            "code" => "Code",
            "raw" => "Raw",
            _ => continue,
        };
        let n = counts.entry(typ).or_insert(0);
        *n += 1;
        let idx = *n;
        let suffix = if typ != "raw" {
            format!(" {idx}")
        } else {
            String::new()
        };
        let src = source_text(cell.get("source"))
            .trim_end_matches('\n')
            .to_owned();
        out.push(format!("# ── {label} cell{suffix} ──"));
        out.push(src);
        out.push(String::new());
        if typ == "code" {
            let rendered = notebook_outputs(cell, &jq_ptr, display_path);
            if !rendered.is_empty() {
                out.push(format!("# ── Output (cell {idx}) ──"));
                out.push(rendered.trim_end_matches('\n').to_owned());
                out.push(String::new());
            }
        }
    }
    joined_lines(&out, "Notebook contains no readable cells")
}

// ── DOCX Extractor ─────────────────────────────────────────────────────────────────────────────

fn collect_docx_paragraph(node: &XmlNode, buf: &mut String) {
    let XmlNode::Element { children, .. } = node else {
        return;
    };
    for child in children {
        match child.local_name() {
            "rt" => {} // ruby phonetic annotation: skip
            "t" | "delText" | "instrText" => buf.push_str(&child.collect_text()),
            "tab" => buf.push('\t'),
            "br" | "cr" => buf.push('\n'),
            _ => collect_docx_paragraph(child, buf),
        }
    }
}

fn extract_docx(data: &[u8]) -> Result<String, String> {
    let zip = ZipArchive::from_bytes(data, "DOCX")?;
    let doc_bytes = zip.read_entry("word/document.xml")?;
    let root = parse_xml(&doc_bytes, "word/document.xml")?;
    let mut paragraphs = Vec::new();
    find_descendants(&root, "p", &mut paragraphs);
    let mut lines = Vec::new();
    for para in paragraphs {
        let mut buf = String::new();
        collect_docx_paragraph(para, &mut buf);
        for part in buf.split('\n') {
            lines.push(part.to_owned());
        }
    }
    joined_lines(&lines, "DOCX contains no extractable text")
}

// ── XLSX Extractor ─────────────────────────────────────────────────────────────────────────────

fn xlsx_string_text(item: &XmlNode) -> String {
    let mut out = String::new();
    for child in item.children() {
        match child.local_name() {
            "t" => out.push_str(&child.collect_text()),
            "r" => {
                for sub in child.children() {
                    if sub.local_name() == "t" {
                        out.push_str(&sub.collect_text());
                    }
                }
            }
            _ => {}
        }
    }
    out
}

fn col_index(cell_ref: &str) -> usize {
    let mut idx = 0usize;
    for ch in cell_ref.chars().take_while(|c| c.is_ascii_alphabetic()) {
        let digit = (ch.to_ascii_uppercase() as usize) - ('A' as usize) + 1;
        idx = idx.saturating_mul(26).saturating_add(digit);
    }
    idx.saturating_sub(1)
}

fn posix_normpath(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    parts.join("/")
}

fn cell_value(cell: &XmlNode, shared: &[String]) -> String {
    let typ = cell.attr("t").unwrap_or("");
    let v = cell
        .children()
        .iter()
        .find(|c| c.local_name() == "v")
        .map(XmlNode::collect_text)
        .unwrap_or_default();
    match typ {
        "s" => v
            .trim()
            .parse::<usize>()
            .ok()
            .and_then(|i| shared.get(i))
            .cloned()
            .unwrap_or_default(),
        "inlineStr" => cell
            .children()
            .iter()
            .find(|c| c.local_name() == "is")
            .map(xlsx_string_text)
            .unwrap_or_default(),
        "b" => {
            if matches!(v.trim(), "1" | "true" | "TRUE") {
                "TRUE".to_owned()
            } else {
                "FALSE".to_owned()
            }
        }
        "e" => {
            if v.is_empty() {
                "#ERROR".to_owned()
            } else {
                v
            }
        }
        _ => v,
    }
}

fn extract_sheet_rows(
    xml_bytes: &[u8],
    shared: &[String],
    part: &str,
) -> Result<Vec<Vec<String>>, String> {
    let root = parse_xml(xml_bytes, part)?;
    let mut row_nodes = Vec::new();
    find_descendants(&root, "row", &mut row_nodes);
    let mut rows: Vec<Vec<String>> = Vec::new();
    for row_node in row_nodes.into_iter().take(MAX_XLSX_ROWS_PER_SHEET) {
        let mut cells: BTreeMap<usize, String> = BTreeMap::new();
        let mut max_col: isize = -1;
        for cell in row_node.children().iter().filter(|c| c.local_name() == "c") {
            let col = match cell.attr("r") {
                Some(r) if !r.is_empty() => col_index(r),
                _ => usize::try_from(max_col + 1).unwrap_or(0),
            };
            if col < MAX_XLSX_COLS {
                cells.insert(col, cell_value(cell, shared));
                max_col = max_col.max(col as isize);
            }
        }
        if max_col >= 0 {
            let mut row = Vec::with_capacity(max_col as usize + 1);
            for i in 0..=(max_col as usize) {
                row.push(cells.remove(&i).unwrap_or_default());
            }
            rows.push(row);
        } else {
            rows.push(Vec::new());
        }
    }
    while rows
        .last()
        .is_some_and(|r| r.iter().all(|c| c.trim().is_empty()))
    {
        rows.pop();
    }
    Ok(rows)
}

fn extract_xlsx(data: &[u8]) -> Result<String, String> {
    let zip = ZipArchive::from_bytes(data, "XLSX")?;
    let mut shared: Vec<String> = Vec::new();
    if zip.contains("xl/sharedStrings.xml") {
        if let Ok(sst_bytes) = zip.read_entry("xl/sharedStrings.xml") {
            if let Ok(sst_root) = parse_xml(&sst_bytes, "xl/sharedStrings.xml") {
                let mut items = Vec::new();
                find_descendants(&sst_root, "si", &mut items);
                for item in items {
                    shared.push(xlsx_string_text(item));
                }
            }
        }
    }

    let mut rels: HashMap<String, String> = HashMap::new();
    if zip.contains("xl/_rels/workbook.xml.rels") {
        if let Ok(rel_bytes) = zip.read_entry("xl/_rels/workbook.xml.rels") {
            if let Ok(rel_root) = parse_xml(&rel_bytes, "xl/_rels/workbook.xml.rels") {
                let mut rel_nodes = Vec::new();
                find_descendants(&rel_root, "Relationship", &mut rel_nodes);
                for rel in rel_nodes {
                    if let (Some(id), Some(target)) = (rel.attr("Id"), rel.attr("Target")) {
                        if !id.is_empty() {
                            rels.insert(id.to_owned(), target.to_owned());
                        }
                    }
                }
            }
        }
    }

    let wb_bytes = zip.read_entry("xl/workbook.xml")?;
    let wb_root = parse_xml(&wb_bytes, "xl/workbook.xml")?;
    let mut sheets = Vec::new();
    find_descendants(&wb_root, "sheet", &mut sheets);

    let mut out = Vec::new();
    for sheet in sheets {
        let state = sheet.attr("state").unwrap_or("visible");
        if matches!(state, "hidden" | "veryHidden") {
            continue;
        }
        let rid = sheet.attr("id").unwrap_or("");
        let target = rels
            .get(rid)
            .map(String::as_str)
            .unwrap_or("")
            .trim_start_matches('/');
        let raw_part = if target.starts_with("xl/") {
            target.to_owned()
        } else {
            format!("xl/{target}")
        };
        let part = posix_normpath(&raw_part);
        if !zip.contains(&part) {
            continue;
        }
        if let Ok(sheet_bytes) = zip.read_entry(&part) {
            if let Ok(rows) = extract_sheet_rows(&sheet_bytes, &shared, &part) {
                let name = sheet.attr("name").unwrap_or("Sheet");
                out.push(format!("# ── Sheet: {name} ──"));
                if rows.is_empty() {
                    out.push("(empty)".to_owned());
                } else {
                    for row in rows {
                        out.push(row.join("\t"));
                    }
                }
                out.push(String::new());
            }
        }
    }
    joined_lines(&out, "XLSX has no visible sheets with content")
}

// ── PPTX Extractor ─────────────────────────────────────────────────────────────────────────────

fn slide_number(name: &str) -> Option<usize> {
    let rest = name.strip_prefix("ppt/slides/slide")?;
    let digits = rest.strip_suffix(".xml")?;
    digits.parse().ok()
}

fn extract_pptx(data: &[u8]) -> Result<String, String> {
    let zip = ZipArchive::from_bytes(data, "PPTX")?;
    let mut slides: Vec<(usize, &str)> = zip
        .entry_names()
        .iter()
        .filter_map(|name| slide_number(name).map(|num| (num, name.as_str())))
        .collect();
    slides.sort_by_key(|(num, _)| *num);
    if slides.is_empty() {
        return Err("PPTX contains no slides".to_owned());
    }
    let mut out = Vec::new();
    for (num, name) in slides {
        let xml_bytes = zip.read_entry(name)?;
        let root = parse_xml(&xml_bytes, name)?;
        let mut paras = Vec::new();
        find_descendants(&root, "p", &mut paras);
        let mut slide_lines = Vec::new();
        for p in paras {
            let mut texts = Vec::new();
            find_descendants(p, "t", &mut texts);
            let joined: String = texts.into_iter().map(XmlNode::collect_text).collect();
            if !joined.trim().is_empty() {
                slide_lines.push(joined);
            }
        }
        out.push(format!("# ── Slide {num} ──"));
        if slide_lines.is_empty() {
            out.push("(empty)".to_owned());
        } else {
            out.extend(slide_lines);
        }
        out.push(String::new());
    }
    joined_lines(&out, "PPTX contains no extractable text")
}

// ── SQLite Extractor (.db / .sqlite / .sqlite3) ────────────────────────────────────────────────

fn sqlite_cell(val: rusqlite::types::ValueRef<'_>) -> String {
    match val {
        rusqlite::types::ValueRef::Null => "NULL".to_owned(),
        rusqlite::types::ValueRef::Integer(n) => n.to_string(),
        rusqlite::types::ValueRef::Real(f) => f.to_string(),
        rusqlite::types::ValueRef::Text(bytes) => {
            let text = String::from_utf8_lossy(bytes)
                .replace('|', "\\|")
                .replace('\n', " ");
            if text.chars().count() <= SQLITE_CELL_CHARS {
                text
            } else {
                let cut: String = text.chars().take(SQLITE_CELL_CHARS - 1).collect();
                format!("{cut}…")
            }
        }
        rusqlite::types::ValueRef::Blob(bytes) => format!("<blob {} bytes>", bytes.len()),
    }
}

fn encode_sqlite_uri_path(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() + 8);
    for ch in raw.chars() {
        match ch {
            '\\' => out.push('/'),
            '%' => out.push_str("%25"),
            '?' => out.push_str("%3F"),
            '#' => out.push_str("%23"),
            ' ' => out.push_str("%20"),
            _ => out.push(ch),
        }
    }
    if out.starts_with('/') {
        format!("file:{out}?mode=ro&immutable=1")
    } else {
        format!("file:///{out}?mode=ro&immutable=1")
    }
}

fn extract_sqlite(data: &[u8]) -> Result<String, String> {
    if data.len() < SQLITE_MAGIC.len() || &data[..SQLITE_MAGIC.len()] != SQLITE_MAGIC {
        return Err("not a SQLite database (magic bytes do not match)".to_owned());
    }
    let mut tmp = tempfile::Builder::new()
        .suffix(".sqlite")
        .tempfile()
        .map_err(|e| format!("SQLite temp file failed: {e}"))?;
    tmp.write_all(data)
        .and_then(|()| tmp.flush())
        .map_err(|e| format!("SQLite temp write failed: {e}"))?;
    let uri = encode_sqlite_uri_path(&tmp.path().to_string_lossy());
    let conn = Connection::open_with_flags(
        &uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| format!("SQLite read failed: {e}"))?;

    let mut stmt = conn
        .prepare("SELECT type, name, sql FROM sqlite_master WHERE sql IS NOT NULL ORDER BY type DESC, name")
        .map_err(|e| format!("SQLite read failed: {e}"))?;
    let objs: Vec<(String, String, String)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .map_err(|e| format!("SQLite read failed: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("SQLite read failed: {e}"))?;

    let tables: Vec<&(String, String, String)> =
        objs.iter().filter(|(t, _, _)| t == "table").collect();
    let others: Vec<&(String, String, String)> =
        objs.iter().filter(|(t, _, _)| t != "table").collect();

    let mut out = vec![
        "# SQLite database".to_owned(),
        String::new(),
        format!(
            "{} table(s), {} index/view/trigger(s)",
            tables.len(),
            others.len()
        ),
    ];

    for (_, name, sql) in tables.iter().take(SQLITE_MAX_TABLES) {
        let quoted = format!("\"{}\"", name.replace('"', "\"\""));
        let count: i64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM {quoted}"), [], |r| r.get(0))
            .map_err(|e| format!("SQLite read failed: {e}"))?;
        out.push(String::new());
        out.push(format!("## {name}  ({count} rows)"));
        out.push(sql.trim().to_owned());

        let mut cur = conn
            .prepare(&format!(
                "SELECT * FROM {quoted} LIMIT {SQLITE_PREVIEW_ROWS}"
            ))
            .map_err(|e| format!("SQLite read failed: {e}"))?;
        let cols: Vec<String> = cur.column_names().iter().map(|s| (*s).to_owned()).collect();
        let col_count = cols.len();
        let mut rows_out: Vec<Vec<String>> = Vec::new();
        let mut iter = cur
            .query([])
            .map_err(|e| format!("SQLite read failed: {e}"))?;
        while let Some(row) = iter
            .next()
            .map_err(|e| format!("SQLite read failed: {e}"))?
        {
            let mut cells = Vec::with_capacity(col_count);
            for i in 0..col_count {
                let val = row
                    .get_ref(i)
                    .map_err(|e| format!("SQLite read failed: {e}"))?;
                cells.push(sqlite_cell(val));
            }
            rows_out.push(cells);
        }
        if !rows_out.is_empty() && col_count > 0 {
            out.push(format!("| {} |", cols.join(" | ")));
            out.push(format!("|{}", "---|".repeat(col_count)));
            for row in rows_out {
                out.push(format!("| {} |", row.join(" | ")));
            }
        }
    }

    if tables.len() > SQLITE_MAX_TABLES {
        out.push(format!(
            "\n... {} more tables omitted",
            tables.len() - SQLITE_MAX_TABLES
        ));
    }
    if !others.is_empty() {
        out.push(String::new());
        out.push("## Indexes / views / triggers".to_owned());
        for (typ, name, _) in others {
            out.push(format!("- {typ} {name}"));
        }
    }
    out.push(String::new());
    out.push(
        "Query it with the terminal: sqlite3 <path> 'SELECT ...'  (or Python's sqlite3 module)."
            .to_owned(),
    );
    Ok(out.join("\n"))
}

// ── PDF Extractor ──────────────────────────────────────────────────────────────────────────────

fn run_pdftotext(path: &Path) -> Option<String> {
    let mut child = Command::new("pdftotext")
        .arg(path)
        .arg("-")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = std::io::Read::read_to_end(&mut std::io::BufReader::new(stdout), &mut buf);
        buf
    });
    let deadline = Instant::now() + PDF_PAGE_SCAN_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(15)),
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let bytes = reader.join().unwrap_or_default();
    if status.is_some_and(|st| st.success()) {
        Some(String::from_utf8_lossy(&bytes).into_owned())
    } else {
        None
    }
}

fn pdf_coverage_note(pages: &[&str], display_path: &str) -> String {
    if pages.len() < 2 {
        return String::new();
    }
    let counts: Vec<usize> = pages.iter().map(|p| p.trim().chars().count()).collect();
    let empty: Vec<usize> = counts
        .iter()
        .enumerate()
        .filter_map(|(i, &n)| (n < PDF_EMPTY_PAGE_CHARS).then_some(i + 1))
        .collect();
    let total = counts.len();
    let n_empty = empty.len();
    let enough = (n_empty as f64 / total as f64) >= PDF_COVERAGE_MIN_RATIO
        || n_empty >= PDF_COVERAGE_ABSOLUTE_EMPTY;
    if n_empty < PDF_COVERAGE_MIN_EMPTY || !enough {
        return String::new();
    }
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for p in empty {
        if let Some(last) = ranges.last_mut().filter(|r| r.1 + 1 == p) {
            last.1 = p;
        } else {
            ranges.push((p, p));
        }
    }
    let mut gap_lines = Vec::new();
    for &(a, b) in ranges.iter().take(PDF_GAP_MAP_MAX_ENTRIES) {
        let mut label = String::new();
        for prev in (0..a.saturating_sub(1)).rev() {
            if counts[prev] >= PDF_EMPTY_PAGE_CHARS {
                let snippet: String = pages[prev]
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
                    .chars()
                    .take(GAP_CONTEXT_CHARS)
                    .collect();
                label = format!(" — after \"{snippet}\" (p{})", prev + 1);
                break;
            }
        }
        let span = if a == b {
            format!("page {a}")
        } else {
            format!("pages {a}-{b}")
        };
        let n = b - a + 1;
        let plural = if n != 1 { "s" } else { "" };
        gap_lines.push(format!("  {span} ({n} page{plural}){label}"));
    }
    if ranges.len() > PDF_GAP_MAP_MAX_ENTRIES {
        let rest = &ranges[PDF_GAP_MAP_MAX_ENTRIES..];
        let rem_pages: usize = rest.iter().map(|(a, b)| b - a + 1).sum();
        gap_lines.push(format!("  … {} more gaps ({rem_pages} pages)", rest.len()));
    }
    format!(
        "[EXTRACTION COVERAGE WARNING: {n_empty} of {total} pages in this PDF yielded no text. \
Those pages are likely scanned images (or blank) — their content is MISSING from the extracted text below, \
even where section headers appear with empty bodies. Unreadable gaps, each labeled with the last text extracted before it:\n\
{}\n\
Decide which gaps you actually need — do NOT OCR or render everything. For the gaps that matter, render just that range with \
`pdftoppm -jpeg -r 150 -f <first> -l <last> {} $TMPDIR/page` and inspect each image with the vision_analyze tool, or use the \
ocr-and-documents skill (marker-pdf) for bulk OCR of large ranges.]\n",
        gap_lines.join("\n"),
        shell_single_quote(display_path)
    )
}

fn find_subslice(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    haystack
        .get(from..)?
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|rel| from + rel)
}

fn decode_pdf_bytes_text(raw: &[u8]) -> String {
    if raw.len() >= 2 && raw[0] == 0xfe && raw[1] == 0xff {
        let utf16: Vec<u16> = raw[2..]
            .chunks_exact(2)
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect();
        return String::from_utf16_lossy(&utf16);
    }
    raw.iter()
        .map(|&b| match b {
            0x20..=0x7e | b'\n' | b'\r' | b'\t' => b as char,
            0xa0..=0xff => b as char,
            _ => ' ',
        })
        .collect()
}

fn parse_pdf_literal_string(data: &[u8], start_paren: usize) -> Option<(String, usize)> {
    let mut idx = start_paren + 1;
    let mut depth = 1usize;
    let mut bytes = Vec::new();
    while idx < data.len() {
        let b = data[idx];
        idx += 1;
        match b {
            b'(' => {
                depth += 1;
                bytes.push(b);
            }
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some((decode_pdf_bytes_text(&bytes), idx));
                }
                bytes.push(b);
            }
            b'\\' => {
                let &esc = data.get(idx)?;
                idx += 1;
                match esc {
                    b'n' => bytes.push(b'\n'),
                    b'r' => bytes.push(b'\r'),
                    b't' => bytes.push(b'\t'),
                    b'b' => bytes.push(0x08),
                    b'f' => bytes.push(0x0c),
                    b'(' | b')' | b'\\' => bytes.push(esc),
                    b'\r' => {
                        if data.get(idx) == Some(&b'\n') {
                            idx += 1;
                        }
                    }
                    b'\n' => {}
                    b'0'..=b'7' => {
                        let mut val = u32::from(esc - b'0');
                        for _ in 0..2 {
                            if let Some(&d) = data.get(idx).filter(|b| (b'0'..=b'7').contains(b)) {
                                val = val * 8 + u32::from(d - b'0');
                                idx += 1;
                            } else {
                                break;
                            }
                        }
                        bytes.push((val & 0xff) as u8);
                    }
                    other => bytes.push(other),
                }
            }
            other => bytes.push(other),
        }
    }
    None
}

fn parse_pdf_hex_string(data: &[u8], start_lt: usize) -> Option<(String, usize)> {
    let mut idx = start_lt + 1;
    let mut nibbles: Vec<u8> = Vec::new();
    while idx < data.len() {
        let b = data[idx];
        idx += 1;
        if b == b'>' {
            if nibbles.len() % 2 == 1 {
                nibbles.push(0);
            }
            let raw: Vec<u8> = nibbles
                .chunks_exact(2)
                .map(|c| (c[0] << 4) | c[1])
                .collect();
            return Some((decode_pdf_bytes_text(&raw), idx));
        }
        let nib = match b {
            b'0'..=b'9' => b - b'0',
            b'a'..=b'f' => b - b'a' + 10,
            b'A'..=b'F' => b - b'A' + 10,
            b' ' | b'\t' | b'\r' | b'\n' => continue,
            _ => return None,
        };
        nibbles.push(nib);
    }
    None
}

fn extract_pdf_text_operators(stream: &[u8], out: &mut String) {
    let mut idx = 0usize;
    let mut in_text = false;
    let mut pending_str: Option<String> = None;
    let mut pending_array: Vec<(bool, String, f64)> = Vec::new();

    while idx < stream.len() {
        let b = stream[idx];
        if b.is_ascii_whitespace() {
            idx += 1;
            continue;
        }
        if b == b'%' {
            while idx < stream.len() && stream[idx] != b'\n' && stream[idx] != b'\r' {
                idx += 1;
            }
            continue;
        }
        if in_text && b == b'(' {
            if let Some((text, next)) = parse_pdf_literal_string(stream, idx) {
                pending_str = Some(text);
                idx = next;
                continue;
            }
        }
        if in_text && b == b'<' && stream.get(idx + 1) != Some(&b'<') {
            if let Some((text, next)) = parse_pdf_hex_string(stream, idx) {
                pending_str = Some(text);
                idx = next;
                continue;
            }
        }
        if in_text && b == b'[' {
            pending_array.clear();
            idx += 1;
            while idx < stream.len() && stream[idx] != b']' {
                let c = stream[idx];
                if c.is_ascii_whitespace() {
                    idx += 1;
                } else if c == b'(' {
                    if let Some((text, next)) = parse_pdf_literal_string(stream, idx) {
                        pending_array.push((true, text, 0.0));
                        idx = next;
                    } else {
                        break;
                    }
                } else if c == b'<' && stream.get(idx + 1) != Some(&b'<') {
                    if let Some((text, next)) = parse_pdf_hex_string(stream, idx) {
                        pending_array.push((true, text, 0.0));
                        idx = next;
                    } else {
                        break;
                    }
                } else if c.is_ascii_digit() || matches!(c, b'-' | b'+' | b'.') {
                    let start = idx;
                    idx += 1;
                    while idx < stream.len()
                        && (stream[idx].is_ascii_digit() || stream[idx] == b'.')
                    {
                        idx += 1;
                    }
                    if let Ok(num) = std::str::from_utf8(&stream[start..idx])
                        .unwrap_or("")
                        .parse::<f64>()
                    {
                        pending_array.push((false, String::new(), num));
                    }
                } else {
                    idx += 1;
                }
            }
            if idx < stream.len() && stream[idx] == b']' {
                idx += 1;
            }
            continue;
        }

        // Token / operator
        let start = idx;
        while idx < stream.len()
            && !stream[idx].is_ascii_whitespace()
            && !matches!(stream[idx], b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'/')
        {
            idx += 1;
        }
        if idx == start {
            idx += 1;
            continue;
        }
        let op = &stream[start..idx];
        match op {
            b"BT" => {
                in_text = true;
                pending_str = None;
                pending_array.clear();
            }
            b"ET" => {
                if in_text && !out.ends_with('\n') && !out.is_empty() {
                    out.push('\n');
                }
                in_text = false;
            }
            b"Tj" if in_text => {
                if let Some(s) = pending_str.take() {
                    out.push_str(&s);
                }
            }
            b"TJ" if in_text => {
                for (is_str, text, adjust) in pending_array.drain(..) {
                    if is_str {
                        out.push_str(&text);
                    } else if adjust <= -120.0 && !out.ends_with([' ', '\n']) {
                        out.push(' ');
                    }
                }
            }
            b"'" | b"\"" if in_text => {
                if !out.ends_with('\n') && !out.is_empty() {
                    out.push('\n');
                }
                if let Some(s) = pending_str.take() {
                    out.push_str(&s);
                }
            }
            b"Td" | b"TD" | b"T*" if in_text && !out.ends_with('\n') && !out.is_empty() => {
                out.push('\n');
            }
            _ => {}
        }
    }
}

fn extract_pdf_pure_rust(data: &[u8]) -> String {
    let mut collected = String::new();
    let mut cursor = 0usize;
    while let Some(stream_pos) = find_subslice(data, b"stream", cursor) {
        let after_kw = stream_pos + 6;
        let payload_start = if data.get(after_kw..after_kw + 2) == Some(b"\r\n") {
            after_kw + 2
        } else if data.get(after_kw) == Some(&b'\n') {
            after_kw + 1
        } else {
            cursor = after_kw;
            continue;
        };
        let Some(end_pos) = find_subslice(data, b"endstream", payload_start) else {
            break;
        };
        let mut raw_stream = &data[payload_start..end_pos];
        if raw_stream.ends_with(b"\r\n") {
            raw_stream = &raw_stream[..raw_stream.len() - 2];
        } else if raw_stream.ends_with(b"\n") || raw_stream.ends_with(b"\r") {
            raw_stream = &raw_stream[..raw_stream.len() - 1];
        }
        let dict_start = stream_pos.saturating_sub(512);
        let dict_slice = &data[dict_start..stream_pos];
        let is_flate = find_subslice(dict_slice, b"/FlateDecode", 0).is_some();
        let is_image = find_subslice(dict_slice, b"/Subtype /Image", 0).is_some()
            || find_subslice(dict_slice, b"/Subtype/Image", 0).is_some();

        if !is_image {
            if is_flate {
                if let Ok(decompressed) = inflate_zlib(raw_stream, MAX_DOCUMENT_BYTES as usize) {
                    extract_pdf_text_operators(&decompressed, &mut collected);
                }
            } else {
                extract_pdf_text_operators(raw_stream, &mut collected);
            }
        }
        cursor = end_pos + 9;
    }
    collected
}

fn extract_pdf(data: &[u8], display_path: &str) -> Result<String, String> {
    if !data.starts_with(b"%PDF-") {
        return Err("Not a valid PDF document (missing %PDF- header)".to_owned());
    }
    if let Ok(mut tmp) = tempfile::Builder::new().suffix(".pdf").tempfile() {
        if tmp.write_all(data).is_ok() && tmp.flush().is_ok() {
            if let Some(raw_text) = run_pdftotext(tmp.path()) {
                let mut pages: Vec<&str> = raw_text.split('\x0c').collect();
                if pages.last().is_some_and(|last| last.trim().is_empty()) {
                    pages.pop();
                }
                let trimmed = raw_text.replace('\x0c', "\n").trim_end().to_owned();
                if !trimmed.trim().is_empty() {
                    let note = pdf_coverage_note(&pages, display_path);
                    return Ok(format!("{note}{trimmed}\n"));
                }
                if !pages.is_empty() {
                    let quoted = shell_single_quote(display_path);
                    return Ok(format!(
                        "[NEEDS OCR: all {} pages of this PDF are scanned images with no text layer — their content is MISSING below. \
If the missing pages matter: render just those pages with `pdftoppm -jpeg -r 150 -f <first> -l <last> {quoted} $TMPDIR/page` \
and inspect via vision_analyze, or check whether an OCR skill is available (skills_list).]\n",
                        pages.len()
                    ));
                }
            }
        }
    }

    let fallback = extract_pdf_pure_rust(data);
    if !fallback.trim().is_empty() {
        return Ok(format!("{}\n", fallback.trim_end()));
    }
    let quoted = shell_single_quote(display_path);
    Ok(format!(
        "[NEEDS OCR: pages unknown of this PDF are scanned images with no text layer (or `pdftotext` is not installed) — their content is MISSING below. \
If the missing pages matter: render just those pages with `pdftoppm -jpeg -r 150 -f <first> -l <last> {quoted} $TMPDIR/page` \
and inspect via vision_analyze, or check whether an OCR skill is available (skills_list).]\n"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn crc32(data: &[u8]) -> u32 {
        let mut crc = 0xffff_ffffu32;
        for &b in data {
            crc ^= u32::from(b);
            for _ in 0..8 {
                let mask = if crc & 1 != 0 { 0xedb8_8320 } else { 0 };
                crc = (crc >> 1) ^ mask;
            }
        }
        !crc
    }

    fn build_stored_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut archive = Vec::new();
        let mut cd = Vec::new();
        for &(name, payload) in entries {
            let offset = archive.len() as u32;
            let crc = crc32(payload);
            let name_bytes = name.as_bytes();
            let size = payload.len() as u32;

            // Local file header
            archive.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
            archive.extend_from_slice(&20u16.to_le_bytes());
            archive.extend_from_slice(&0u16.to_le_bytes());
            archive.extend_from_slice(&0u16.to_le_bytes()); // method 0 stored
            archive.extend_from_slice(&0u16.to_le_bytes());
            archive.extend_from_slice(&0u16.to_le_bytes());
            archive.extend_from_slice(&crc.to_le_bytes());
            archive.extend_from_slice(&size.to_le_bytes());
            archive.extend_from_slice(&size.to_le_bytes());
            archive.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
            archive.extend_from_slice(&0u16.to_le_bytes());
            archive.extend_from_slice(name_bytes);
            archive.extend_from_slice(payload);

            // Central directory header
            cd.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
            cd.extend_from_slice(&20u16.to_le_bytes());
            cd.extend_from_slice(&20u16.to_le_bytes());
            cd.extend_from_slice(&0u16.to_le_bytes());
            cd.extend_from_slice(&0u16.to_le_bytes());
            cd.extend_from_slice(&0u16.to_le_bytes());
            cd.extend_from_slice(&0u16.to_le_bytes());
            cd.extend_from_slice(&crc.to_le_bytes());
            cd.extend_from_slice(&size.to_le_bytes());
            cd.extend_from_slice(&size.to_le_bytes());
            cd.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
            cd.extend_from_slice(&0u16.to_le_bytes());
            cd.extend_from_slice(&0u16.to_le_bytes());
            cd.extend_from_slice(&0u16.to_le_bytes());
            cd.extend_from_slice(&0u16.to_le_bytes());
            cd.extend_from_slice(&0u32.to_le_bytes());
            cd.extend_from_slice(&offset.to_le_bytes());
            cd.extend_from_slice(name_bytes);
        }
        let cd_offset = archive.len() as u32;
        let cd_size = cd.len() as u32;
        archive.extend_from_slice(&cd);
        // EOCD
        archive.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        archive.extend_from_slice(&0u16.to_le_bytes());
        archive.extend_from_slice(&0u16.to_le_bytes());
        archive.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        archive.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        archive.extend_from_slice(&cd_size.to_le_bytes());
        archive.extend_from_slice(&cd_offset.to_le_bytes());
        archive.extend_from_slice(&0u16.to_le_bytes());
        archive
    }

    #[test]
    fn inflate_stored_and_fixed_huffman_and_zlib() {
        // Uncompressed DEFLATE stored block for "hello"
        let stored = [0x01, 0x05, 0x00, 0xfa, 0xff, b'h', b'e', b'l', b'l', b'o'];
        assert_eq!(inflate_raw(&stored, 1024).unwrap(), b"hello");

        // Standard zlib stream of "Hello world!" produced by zlib level 6
        let zlib_hello: &[u8] = &[
            0x78, 0x9c, 0xf3, 0x48, 0xcd, 0xc9, 0xc9, 0x57, 0x28, 0xcf, 0x2f, 0xca, 0x49, 0x51,
            0x04, 0x00, 0x1c, 0x49, 0x04, 0x3e,
        ];
        assert_eq!(inflate_zlib(zlib_hello, 1024).unwrap(), b"Hello world!");
    }

    #[test]
    fn extracts_notebook_v4_and_v3() {
        let nb = serde_json::json!({
            "cells": [
                {"cell_type": "markdown", "source": ["# Title\n", "Body"]},
                {"cell_type": "code", "source": "print(1 + 1)", "outputs": [
                    {"output_type": "stream", "text": "2\n"},
                    {"output_type": "error", "ename": "ValueError", "evalue": "bad", "traceback": ["line 1"]}
                ]},
                {"cell_type": "raw", "source": "raw line"}
            ]
        });
        let extracted = extract_notebook(nb.to_string().as_bytes(), "demo.ipynb").unwrap();
        assert!(extracted.contains("# ── Markdown cell 1 ──"));
        assert!(extracted.contains("# ── Code cell 1 ──"));
        assert!(extracted.contains("# ── Output (cell 1) ──"));
        assert!(extracted.contains("Error: ValueError: bad"));
        assert!(extracted.contains("# ── Raw cell ──"));
    }

    #[test]
    fn extracts_docx_xlsx_and_pptx() {
        let docx = build_stored_zip(&[(
            "word/document.xml",
            br#"<?xml version="1.0" encoding="UTF-8"?>
            <w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
              <w:body>
                <w:p><w:r><w:t>First</w:t><w:tab/><w:t>Column</w:t></w:r></w:p>
                <w:p><w:r><w:ruby><w:rubyBase><w:t>Kanji</w:t></w:rubyBase><w:rt><w:t>kana</w:t></w:rt></w:ruby></w:r><w:br/><w:r><w:t>Next</w:t></w:r></w:p>
              </w:body>
            </w:document>"#,
        )]);
        let docx_text = extract_docx(&docx).unwrap();
        assert_eq!(docx_text, "First\tColumn\nKanji\nNext\n");

        let xlsx = build_stored_zip(&[
            (
                "xl/workbook.xml",
                br#"<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
                    <sheets>
                        <sheet name="Summary" sheetId="1" r:id="rId1"/>
                        <sheet name="Hidden" sheetId="2" state="hidden" r:id="rId2"/>
                    </sheets>
                </workbook>"#,
            ),
            (
                "xl/_rels/workbook.xml.rels",
                br#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
                    <Relationship Id="rId1" Target="worksheets/sheet1.xml"/>
                    <Relationship Id="rId2" Target="worksheets/sheet2.xml"/>
                </Relationships>"#,
            ),
            (
                "xl/sharedStrings.xml",
                br#"<sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
                    <si><t>Revenue</t></si>
                </sst>"#,
            ),
            (
                "xl/worksheets/sheet1.xml",
                br#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
                    <sheetData>
                        <row r="1"><c r="A1" t="s"><v>0</v></c><c r="C1" t="inlineStr"><is><t>Flag</t></is></c></row>
                        <row r="2"><c r="A2"><v>4200</v></c><c r="C2" t="b"><v>1</v></c><c r="AB2" t="e"><v>#DIV/0!</v></c></row>
                    </sheetData>
                </worksheet>"#,
            ),
            (
                "xl/worksheets/sheet2.xml",
                br#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1"><v>999</v></c></row></sheetData></worksheet>"#,
            ),
        ]);
        let xlsx_text = extract_xlsx(&xlsx).unwrap();
        assert!(xlsx_text.contains("# ── Sheet: Summary ──"));
        assert!(!xlsx_text.contains("Hidden"));
        assert!(xlsx_text.contains("Revenue\t\tFlag"));
        assert!(xlsx_text.contains("4200\t\tTRUE"));
        assert!(xlsx_text.contains("#DIV/0!"));

        let pptx = build_stored_zip(&[
            (
                "ppt/slides/slide2.xml",
                br#"<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"><p:cSld><p:spTree><p:sp><p:txBody><a:p><a:r><a:t>Second Slide</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:sld>"#,
            ),
            (
                "ppt/slides/slide1.xml",
                br#"<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"><p:cSld><p:spTree><p:sp><p:txBody><a:p><a:r><a:t>First Slide</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:sld>"#,
            ),
        ]);
        let pptx_text = extract_pptx(&pptx).unwrap();
        let first_idx = pptx_text.find("# ── Slide 1 ──").unwrap();
        let second_idx = pptx_text.find("# ── Slide 2 ──").unwrap();
        assert!(first_idx < second_idx);
        assert!(pptx_text.contains("First Slide") && pptx_text.contains("Second Slide"));
    }

    #[test]
    fn extracts_sqlite_and_rejects_bad_magic() {
        assert!(extract_sqlite(b"not a database").is_err());

        let tmp = tempfile::NamedTempFile::new().unwrap();
        {
            let conn = Connection::open(tmp.path()).unwrap();
            conn.execute_batch(
                "CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT, payload BLOB);
                 INSERT INTO items VALUES (1, 'alpha|beta', X'010203');
                 CREATE INDEX idx_items_name ON items(name);",
            )
            .unwrap();
        }
        let bytes = std::fs::read(tmp.path()).unwrap();
        let extracted = extract_sqlite(&bytes).unwrap();
        assert!(extracted.contains("# SQLite database"));
        assert!(extracted.contains("## items  (1 rows)"));
        assert!(extracted.contains("alpha\\|beta"));
        assert!(extracted.contains("<blob 3 bytes>"));
        assert!(extracted.contains("- index idx_items_name"));
    }

    #[test]
    fn extracts_pdf_text_and_scanned_fallback() {
        let minimal_pdf = b"%PDF-1.4\n1 0 obj\n<< /Length 36 >>\nstream\nBT\n(Hello from PDF!) Tj\nET\nendstream\nendobj\n%%EOF\n";
        let extracted = extract_pdf_pure_rust(minimal_pdf);
        assert!(extracted.contains("Hello from PDF!"));

        let scanned_pdf =
            b"%PDF-1.4\n1 0 obj\n<< /Length 0 >>\nstream\n\nendstream\nendobj\n%%EOF\n";
        let warn = extract_pdf(scanned_pdf, "scan.pdf").unwrap();
        assert!(warn.starts_with("[NEEDS OCR:"));
    }
}
