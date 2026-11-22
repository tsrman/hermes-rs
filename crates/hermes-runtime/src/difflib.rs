//! `difflib.SequenceMatcher(None, a, b)` over characters, including the popular-element
//! heuristic Python applies to sequences of 200 or more elements.

use std::collections::HashMap;
use std::hash::Hash;

pub(crate) struct SequenceMatcher<'a, T> {
    a: &'a [T],
    b: &'a [T],
    b2j: HashMap<T, Vec<usize>>,
}

pub(crate) type Block = (usize, usize, usize);
pub(crate) type Opcode = (&'static str, usize, usize, usize, usize);

impl<'a, T: Eq + Hash + Copy> SequenceMatcher<'a, T> {
    pub(crate) fn new(a: &'a [T], b: &'a [T]) -> Self {
        let mut b2j: HashMap<T, Vec<usize>> = HashMap::new();
        for (index, element) in b.iter().enumerate() {
            b2j.entry(*element).or_default().push(index);
        }
        // Elements that fill more than 1% of a long sequence only count by extension.
        if b.len() >= 200 {
            let ntest = b.len() / 100 + 1;
            b2j.retain(|_, indexes| indexes.len() <= ntest);
        }
        Self { a, b, b2j }
    }

    fn longest(&self, alo: usize, ahi: usize, blo: usize, bhi: usize) -> Block {
        let (mut besti, mut bestj, mut bestsize) = (alo, blo, 0usize);
        let mut j2len: HashMap<usize, usize> = HashMap::new();
        for i in alo..ahi {
            let mut newj2len: HashMap<usize, usize> = HashMap::new();
            if let Some(indexes) = self.b2j.get(&self.a[i]) {
                for &j in indexes {
                    if j < blo {
                        continue;
                    }
                    if j >= bhi {
                        break;
                    }
                    let k = j2len.get(&j.wrapping_sub(1)).copied().unwrap_or(0) + 1;
                    newj2len.insert(j, k);
                    if k > bestsize {
                        besti = i + 1 - k;
                        bestj = j + 1 - k;
                        bestsize = k;
                    }
                }
            }
            j2len = newj2len;
        }
        // Extend through elements the popularity filter removed.
        while besti > alo && bestj > blo && self.a[besti - 1] == self.b[bestj - 1] {
            besti -= 1;
            bestj -= 1;
            bestsize += 1;
        }
        while besti + bestsize < ahi
            && bestj + bestsize < bhi
            && self.a[besti + bestsize] == self.b[bestj + bestsize]
        {
            bestsize += 1;
        }
        (besti, bestj, bestsize)
    }

    pub(crate) fn matching_blocks(&self) -> Vec<Block> {
        let (la, lb) = (self.a.len(), self.b.len());
        let mut queue = vec![(0, la, 0, lb)];
        let mut blocks: Vec<Block> = Vec::new();
        while let Some((alo, ahi, blo, bhi)) = queue.pop() {
            let (i, j, k) = self.longest(alo, ahi, blo, bhi);
            if k > 0 {
                blocks.push((i, j, k));
                if alo < i && blo < j {
                    queue.push((alo, i, blo, j));
                }
                if i + k < ahi && j + k < bhi {
                    queue.push((i + k, ahi, j + k, bhi));
                }
            }
        }
        blocks.sort_unstable();
        let (mut i1, mut j1, mut k1) = (0usize, 0usize, 0usize);
        let mut merged: Vec<Block> = Vec::new();
        for (i2, j2, k2) in blocks {
            if i1 + k1 == i2 && j1 + k1 == j2 {
                k1 += k2;
            } else {
                if k1 > 0 {
                    merged.push((i1, j1, k1));
                }
                (i1, j1, k1) = (i2, j2, k2);
            }
        }
        if k1 > 0 {
            merged.push((i1, j1, k1));
        }
        merged.push((la, lb, 0));
        merged
    }

    pub(crate) fn ratio(&self) -> f64 {
        let total = self.a.len() + self.b.len();
        if total == 0 {
            return 1.0;
        }
        let matches: usize = self.matching_blocks().iter().map(|block| block.2).sum();
        2.0 * matches as f64 / total as f64
    }

    pub(crate) fn opcodes(&self) -> Vec<Opcode> {
        let (mut i, mut j) = (0usize, 0usize);
        let mut out: Vec<Opcode> = Vec::new();
        for (ai, bj, size) in self.matching_blocks() {
            let tag = if i < ai && j < bj {
                Some("replace")
            } else if i < ai {
                Some("delete")
            } else if j < bj {
                Some("insert")
            } else {
                None
            };
            if let Some(tag) = tag {
                out.push((tag, i, ai, j, bj));
            }
            (i, j) = (ai + size, bj + size);
            if size > 0 {
                out.push(("equal", ai, i, bj, j));
            }
        }
        out
    }

    /// Opcodes in groups with `context` lines of equal text around each change.
    pub(crate) fn grouped_opcodes(&self, context: usize) -> Vec<Vec<Opcode>> {
        let mut codes = self.opcodes();
        if codes.is_empty() {
            codes.push(("equal", 0, 1, 0, 1));
        }
        if codes[0].0 == "equal" {
            let (tag, i1, i2, j1, j2) = codes[0];
            codes[0] = (
                tag,
                i1.max(i2.saturating_sub(context)),
                i2,
                j1.max(j2.saturating_sub(context)),
                j2,
            );
        }
        let last = codes.len() - 1;
        if codes[last].0 == "equal" {
            let (tag, i1, i2, j1, j2) = codes[last];
            codes[last] = (tag, i1, i2.min(i1 + context), j1, j2.min(j1 + context));
        }
        let twice = context * 2;
        let mut groups: Vec<Vec<Opcode>> = Vec::new();
        let mut group: Vec<Opcode> = Vec::new();
        for (tag, mut i1, i2, mut j1, j2) in codes {
            if tag == "equal" && i2 - i1 > twice {
                group.push((tag, i1, i2.min(i1 + context), j1, j2.min(j1 + context)));
                groups.push(std::mem::take(&mut group));
                i1 = i1.max(i2.saturating_sub(context));
                j1 = j1.max(j2.saturating_sub(context));
            }
            group.push((tag, i1, i2, j1, j2));
        }
        if !(group.is_empty() || group.len() == 1 && group[0].0 == "equal") {
            groups.push(group);
        }
        groups
    }
}

/// Python's `str.splitlines(keepends=True)`.
pub(crate) fn split_lines_keepends(text: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut iter = text.char_indices().peekable();
    while let Some((index, c)) = iter.next() {
        if matches!(
            c,
            '\n' | '\r'
                | '\x0b'
                | '\x0c'
                | '\x1c'
                | '\x1d'
                | '\x1e'
                | '\u{85}'
                | '\u{2028}'
                | '\u{2029}'
        ) {
            let mut end = index + c.len_utf8();
            if c == '\r' && iter.peek().is_some_and(|(_, after)| *after == '\n') {
                iter.next();
                end += 1;
            }
            lines.push(&text[start..end]);
            start = end;
        }
    }
    if start < text.len() {
        lines.push(&text[start..]);
    }
    lines
}

fn format_range(start: usize, stop: usize) -> String {
    let mut beginning = start + 1;
    let length = stop - start;
    if length == 1 {
        return beginning.to_string();
    }
    if length == 0 {
        beginning -= 1;
    }
    format!("{beginning},{length}")
}

/// `difflib.unified_diff` with three lines of context, joined as the reference joins it.
pub(crate) fn unified_diff(old: &str, new: &str, from_file: &str, to_file: &str) -> String {
    let a = split_lines_keepends(old);
    let b = split_lines_keepends(new);
    let mut out = String::new();
    for (index, group) in SequenceMatcher::new(&a, &b)
        .grouped_opcodes(3)
        .iter()
        .enumerate()
    {
        if index == 0 {
            out.push_str(&format!("--- {from_file}\n+++ {to_file}\n"));
        }
        let (first, last) = (group[0], group[group.len() - 1]);
        out.push_str(&format!(
            "@@ -{} +{} @@\n",
            format_range(first.1, last.2),
            format_range(first.3, last.4)
        ));
        for (tag, i1, i2, j1, j2) in group {
            if *tag == "equal" {
                for line in &a[*i1..*i2] {
                    out.push(' ');
                    out.push_str(line);
                }
                continue;
            }
            if matches!(*tag, "replace" | "delete") {
                for line in &a[*i1..*i2] {
                    out.push('-');
                    out.push_str(line);
                }
            }
            if matches!(*tag, "replace" | "insert") {
                for line in &b[*j1..*j2] {
                    out.push('+');
                    out.push_str(line);
                }
            }
        }
    }
    out
}

pub(crate) fn ratio(a: &str, b: &str) -> f64 {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    SequenceMatcher::new(&a, &b).ratio()
}
