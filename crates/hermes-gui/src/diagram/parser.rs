use std::collections::HashMap;

use super::{
    EdgeStyle, FlowDirection, FlowEdge, FlowGraph, FlowNode, FlowSubgraph, NodeShape, PieChart,
    PieSlice, SeqEvent, SeqParticipant, SequenceDiagram, XyChart,
};

// ---------------------------------------------------------------------------
// 1. Flowchart / State / ER parsers
// ---------------------------------------------------------------------------

fn parse_direction_token(tok: &str) -> FlowDirection {
    match tok.trim().to_ascii_uppercase().as_str() {
        "LR" => FlowDirection::LeftRight,
        "RL" => FlowDirection::RightLeft,
        "BT" => FlowDirection::BottomUp,
        _ => FlowDirection::TopDown,
    }
}

pub(super) fn parse_flowchart(lines: &[&str], kind_label: &'static str) -> FlowGraph {
    let dir = lines
        .first()
        .and_then(|l| l.split_whitespace().nth(1))
        .map(parse_direction_token)
        .unwrap_or(FlowDirection::TopDown);

    let mut nodes: Vec<FlowNode> = Vec::new();
    let mut id_map: HashMap<String, usize> = HashMap::new();
    let mut edges: Vec<FlowEdge> = Vec::new();
    let mut subgraphs: Vec<FlowSubgraph> = Vec::new();
    let mut active_sg: Vec<usize> = Vec::new();
    let mut direction = dir;

    let ensure_node = |spec: &str,
                       nodes: &mut Vec<FlowNode>,
                       id_map: &mut HashMap<String, usize>,
                       subgraphs: &mut [FlowSubgraph],
                       sg: Option<usize>|
     -> Option<usize> {
        let (id, label, shape) = parse_node_spec(spec)?;
        if let Some(&idx) = id_map.get(&id) {
            if label != id {
                nodes[idx].label = label;
                nodes[idx].shape = shape;
            }
            if nodes[idx].subgraph.is_none() {
                nodes[idx].subgraph = sg;
                if let Some(s) = sg {
                    if !subgraphs[s].nodes.contains(&idx) {
                        subgraphs[s].nodes.push(idx);
                    }
                }
            }
            Some(idx)
        } else {
            let idx = nodes.len();
            nodes.push(FlowNode {
                id: id.clone(),
                label,
                shape,
                subgraph: sg,
            });
            id_map.insert(id, idx);
            if let Some(s) = sg {
                subgraphs[s].nodes.push(idx);
            }
            Some(idx)
        }
    };

    for &raw_line in lines.iter().skip(1) {
        // Split statements separated by `;` (outside quotes)
        for stmt in split_statements(raw_line) {
            let s = stmt.trim();
            if s.is_empty() {
                continue;
            }
            let lower = s.to_ascii_lowercase();
            if lower.starts_with("classdef ")
                || lower.starts_with("class ")
                || lower.starts_with("style ")
                || lower.starts_with("linkstyle ")
                || lower.starts_with("click ")
            {
                continue;
            }
            if let Some(rest) = lower.strip_prefix("direction ") {
                direction = parse_direction_token(rest);
                continue;
            }
            if lower == "end" {
                active_sg.pop();
                continue;
            }
            if lower.starts_with("subgraph ") || lower == "subgraph" {
                let raw_title = s[8..].trim();
                let title = clean_subgraph_title(raw_title);
                let sg_idx = subgraphs.len();
                subgraphs.push(FlowSubgraph {
                    title,
                    nodes: Vec::new(),
                });
                active_sg.push(sg_idx);
                continue;
            }

            let sg = active_sg.last().copied();
            let segments = split_flow_chain(s);
            if segments.len() >= 2 {
                // Chain: node_group_0 --(edge_0)--> node_group_1 --(edge_1)--> ...
                for pair in segments.windows(2) {
                    let from_group = &pair[0].0;
                    let edge_spec = pair[0].1.as_ref();
                    let to_group = &pair[1].0;
                    let Some(es) = edge_spec else { continue };

                    let from_ids: Vec<usize> = from_group
                        .split('&')
                        .filter_map(|part| {
                            ensure_node(part.trim(), &mut nodes, &mut id_map, &mut subgraphs, sg)
                        })
                        .collect();
                    let to_ids: Vec<usize> = to_group
                        .split('&')
                        .filter_map(|part| {
                            ensure_node(part.trim(), &mut nodes, &mut id_map, &mut subgraphs, sg)
                        })
                        .collect();
                    for &f in &from_ids {
                        for &t in &to_ids {
                            edges.push(FlowEdge {
                                from: f,
                                to: t,
                                label: es.label.clone(),
                                style: es.style,
                                arrow_end: es.arrow_end,
                                arrow_start: es.arrow_start,
                            });
                        }
                    }
                }
            } else {
                let _ = ensure_node(s, &mut nodes, &mut id_map, &mut subgraphs, sg);
            }
        }
    }

    FlowGraph {
        kind_label,
        direction,
        nodes,
        edges,
        subgraphs,
    }
}

pub(super) fn parse_state_diagram(lines: &[&str]) -> FlowGraph {
    let mut nodes: Vec<FlowNode> = Vec::new();
    let mut id_map: HashMap<String, usize> = HashMap::new();
    let mut edges: Vec<FlowEdge> = Vec::new();
    let mut direction = FlowDirection::TopDown;
    let mut start_counter = 0usize;
    let mut end_counter = 0usize;

    let ensure_state = |raw_id: &str,
                        is_source: bool,
                        nodes: &mut Vec<FlowNode>,
                        id_map: &mut HashMap<String, usize>,
                        start_counter: &mut usize,
                        end_counter: &mut usize|
     -> usize {
        let trimmed = raw_id.trim();
        if trimmed == "[*]" {
            let (id, shape) = if is_source {
                *start_counter += 1;
                (format!("__start_{start_counter}"), NodeShape::StartDot)
            } else {
                *end_counter += 1;
                (format!("__end_{end_counter}"), NodeShape::EndDot)
            };
            let idx = nodes.len();
            nodes.push(FlowNode {
                id,
                label: String::new(),
                shape,
                subgraph: None,
            });
            return idx;
        }
        if let Some(&idx) = id_map.get(trimmed) {
            idx
        } else {
            let idx = nodes.len();
            nodes.push(FlowNode {
                id: trimmed.to_owned(),
                label: trimmed.to_owned(),
                shape: NodeShape::Rounded,
                subgraph: None,
            });
            id_map.insert(trimmed.to_owned(), idx);
            idx
        }
    };

    for &line in lines.iter().skip(1) {
        let lower = line.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("direction ") {
            direction = parse_direction_token(rest);
            continue;
        }
        if lower.starts_with("note ") || lower == "end note" || line == "{" || line == "}" {
            continue;
        }
        // `state "Display Name" as Alias`
        if let Some(rest) = line
            .strip_prefix("state ")
            .or_else(|| line.strip_prefix("State "))
        {
            if let Some((lhs, alias)) = rest.rsplit_once(" as ") {
                let alias = alias.trim();
                let label = trim_quotes(lhs.trim());
                if let Some(&idx) = id_map.get(alias) {
                    nodes[idx].label = label;
                } else {
                    let idx = nodes.len();
                    nodes.push(FlowNode {
                        id: alias.to_owned(),
                        label,
                        shape: NodeShape::Rounded,
                        subgraph: None,
                    });
                    id_map.insert(alias.to_owned(), idx);
                }
                continue;
            }
        }
        if let Some((lhs, rhs)) = line.split_once("-->") {
            let (target, label) = if let Some((t, l)) = rhs.split_once(':') {
                (
                    t.trim(),
                    Some(l.trim().to_owned()).filter(|s| !s.is_empty()),
                )
            } else {
                (rhs.trim(), None)
            };
            let from = ensure_state(
                lhs,
                true,
                &mut nodes,
                &mut id_map,
                &mut start_counter,
                &mut end_counter,
            );
            let to = ensure_state(
                target,
                false,
                &mut nodes,
                &mut id_map,
                &mut start_counter,
                &mut end_counter,
            );
            edges.push(FlowEdge {
                from,
                to,
                label,
                style: EdgeStyle::Solid,
                arrow_end: true,
                arrow_start: false,
            });
        } else if let Some((id, desc)) = line.split_once(':') {
            let id = id.trim();
            if !id.contains(' ') && !id.is_empty() {
                let idx = ensure_state(
                    id,
                    true,
                    &mut nodes,
                    &mut id_map,
                    &mut start_counter,
                    &mut end_counter,
                );
                let node_id = nodes[idx].id.clone();
                nodes[idx].label = format!("{node_id}\n{}", desc.trim());
            }
        }
    }

    FlowGraph {
        kind_label: "State Diagram",
        direction,
        nodes,
        edges,
        subgraphs: Vec::new(),
    }
}

pub(super) fn parse_er_diagram(lines: &[&str]) -> FlowGraph {
    let mut nodes: Vec<FlowNode> = Vec::new();
    let mut id_map: HashMap<String, usize> = HashMap::new();
    let mut edges: Vec<FlowEdge> = Vec::new();
    let mut in_entity_block = false;

    let mut ensure_entity = |name: &str| -> usize {
        let key = trim_quotes(name.trim());
        if let Some(&idx) = id_map.get(&key) {
            idx
        } else {
            let idx = nodes.len();
            nodes.push(FlowNode {
                id: key.clone(),
                label: key.clone(),
                shape: NodeShape::Rect,
                subgraph: None,
            });
            id_map.insert(key, idx);
            idx
        }
    };

    for &line in lines.iter().skip(1) {
        if in_entity_block {
            if line.contains('}') {
                in_entity_block = false;
            }
            continue;
        }
        if line.ends_with('{') {
            let name = line.trim_end_matches('{').trim();
            if !name.is_empty() {
                ensure_entity(name);
            }
            in_entity_block = true;
            continue;
        }
        // Relationship: `CUSTOMER ||--o{ ORDER : places`
        if let Some((rel_part, label_part)) = line.split_once(':') {
            let tokens: Vec<&str> = rel_part.split_whitespace().collect();
            if tokens.len() >= 3 {
                let from = ensure_entity(tokens[0]);
                let to = ensure_entity(tokens[tokens.len() - 1]);
                let conn = tokens[1..tokens.len() - 1].join("");
                let dashed = conn.contains("..");
                let label = trim_quotes(label_part.trim());
                edges.push(FlowEdge {
                    from,
                    to,
                    label: (!label.is_empty()).then_some(label),
                    style: if dashed {
                        EdgeStyle::Dashed
                    } else {
                        EdgeStyle::Solid
                    },
                    arrow_end: true,
                    arrow_start: false,
                });
            }
        }
    }

    FlowGraph {
        kind_label: "ER Diagram",
        direction: FlowDirection::LeftRight,
        nodes,
        edges,
        subgraphs: Vec::new(),
    }
}

#[derive(Clone, Debug)]
struct EdgeSpec {
    label: Option<String>,
    style: EdgeStyle,
    arrow_end: bool,
    arrow_start: bool,
}

fn split_statements(line: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut in_quote = false;
    for (i, ch) in line.char_indices() {
        match ch {
            '"' => in_quote = !in_quote,
            ';' if !in_quote => {
                out.push(&line[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&line[start..]);
    out
}

fn clean_subgraph_title(raw: &str) -> String {
    let trimmed = raw.trim();
    if let Some(open) = trimmed.find('[') {
        if let Some(close) = trimmed.rfind(']') {
            if close > open {
                return trim_quotes(&trimmed[open + 1..close]);
            }
        }
    }
    trim_quotes(trimmed)
}

fn trim_quotes(s: &str) -> String {
    let s = s.trim();
    if (s.starts_with('"') && s.ends_with('"') && s.len() >= 2)
        || (s.starts_with('\'') && s.ends_with('\'') && s.len() >= 2)
    {
        s[1..s.len() - 1].to_owned()
    } else {
        s.to_owned()
    }
}

fn parse_node_spec(raw: &str) -> Option<(String, String, NodeShape)> {
    // Strip trailing `:::className`
    let raw = raw.split(":::").next().unwrap_or(raw).trim();
    if raw.is_empty() {
        return None;
    }

    let id_end = raw.find(['[', '(', '{', '>']).unwrap_or(raw.len());
    let id = raw[..id_end].trim();
    if id.is_empty() {
        return None;
    }
    let rest = raw[id_end..].trim();
    if rest.is_empty() {
        return Some((id.to_owned(), id.to_owned(), NodeShape::Rect));
    }

    let (shape, inner) = if let Some(s) = rest.strip_prefix("([") {
        (NodeShape::Stadium, s.strip_suffix("])").unwrap_or(s))
    } else if let Some(s) = rest.strip_prefix("[(") {
        (NodeShape::Database, s.strip_suffix(")]").unwrap_or(s))
    } else if let Some(s) = rest.strip_prefix("[[") {
        (NodeShape::Subroutine, s.strip_suffix("]]").unwrap_or(s))
    } else if let Some(s) = rest.strip_prefix("((") {
        (NodeShape::Circle, s.strip_suffix("))").unwrap_or(s))
    } else if let Some(s) = rest.strip_prefix("{{") {
        (NodeShape::Diamond, s.strip_suffix("}}").unwrap_or(s))
    } else if let Some(s) = rest.strip_prefix('[') {
        (NodeShape::Rect, s.strip_suffix(']').unwrap_or(s))
    } else if let Some(s) = rest.strip_prefix('(') {
        (NodeShape::Rounded, s.strip_suffix(')').unwrap_or(s))
    } else if let Some(s) = rest.strip_prefix('{') {
        (NodeShape::Diamond, s.strip_suffix('}').unwrap_or(s))
    } else if let Some(s) = rest.strip_prefix('>') {
        (NodeShape::Rounded, s.strip_suffix(']').unwrap_or(s))
    } else {
        (NodeShape::Rect, rest)
    };

    let label = clean_diagram_label(inner);
    Some((
        id.to_owned(),
        if label.is_empty() {
            id.to_owned()
        } else {
            label
        },
        shape,
    ))
}

fn clean_diagram_label(raw: &str) -> String {
    let unquoted = trim_quotes(raw);
    unquoted
        .replace("<br/>", "\n")
        .replace("<br />", "\n")
        .replace("<br>", "\n")
        .replace("\\n", "\n")
        .trim()
        .to_owned()
}

/// Split a flowchart line like `A[Start] -->|Yes| B{Check} -.-> C((Done))` into `(node_expr, Option<EdgeSpec>)` segments.
fn split_flow_chain(line: &str) -> Vec<(String, Option<EdgeSpec>)> {
    let bytes = line.as_bytes();
    let len = line.len();
    let mut segments: Vec<(String, Option<EdgeSpec>)> = Vec::new();
    let mut seg_start = 0;
    let mut i = 0;
    let mut bracket_depth = 0i32;
    let mut in_quote = false;

    while i < len {
        let b = bytes[i];
        if b == b'"' {
            in_quote = !in_quote;
            i += 1;
            continue;
        }
        if !in_quote {
            match b {
                b'[' | b'(' | b'{' => bracket_depth += 1,
                b']' | b')' | b'}' => bracket_depth = (bracket_depth - 1).max(0),
                _ => {}
            }
            if bracket_depth == 0 {
                if let Some((edge, edge_end)) = match_flow_edge_at(line, i) {
                    let node_part = line[seg_start..i].trim().to_owned();
                    segments.push((node_part, Some(edge)));
                    seg_start = edge_end;
                    i = edge_end;
                    continue;
                }
            }
        }
        let ch = line[i..].chars().next().unwrap();
        i += ch.len_utf8();
    }

    let tail = line[seg_start..].trim().to_owned();
    if !tail.is_empty() || !segments.is_empty() {
        segments.push((tail, None));
    }
    segments
}

fn match_flow_edge_at(line: &str, start: usize) -> Option<(EdgeSpec, usize)> {
    let rest = &line[start..];

    // Inline labeled edges: `-- label -->`, `-. label .->`, `== label ==>`
    if let Some(after) = rest.strip_prefix("-- ") {
        if let Some(end_idx) = after.find("-->") {
            let label = clean_diagram_label(&after[..end_idx]);
            return Some((
                EdgeSpec {
                    label: (!label.is_empty()).then_some(label),
                    style: EdgeStyle::Solid,
                    arrow_end: true,
                    arrow_start: false,
                },
                start + 3 + end_idx + 3,
            ));
        }
        if let Some(end_idx) = after.find("---") {
            let label = clean_diagram_label(&after[..end_idx]);
            return Some((
                EdgeSpec {
                    label: (!label.is_empty()).then_some(label),
                    style: EdgeStyle::Solid,
                    arrow_end: false,
                    arrow_start: false,
                },
                start + 3 + end_idx + 3,
            ));
        }
    }
    if let Some(after) = rest.strip_prefix("-. ") {
        if let Some(end_idx) = after.find(".->") {
            let label = clean_diagram_label(&after[..end_idx]);
            return Some((
                EdgeSpec {
                    label: (!label.is_empty()).then_some(label),
                    style: EdgeStyle::Dashed,
                    arrow_end: true,
                    arrow_start: false,
                },
                start + 3 + end_idx + 3,
            ));
        }
    }
    if let Some(after) = rest.strip_prefix("== ") {
        if let Some(end_idx) = after.find("==>") {
            let label = clean_diagram_label(&after[..end_idx]);
            return Some((
                EdgeSpec {
                    label: (!label.is_empty()).then_some(label),
                    style: EdgeStyle::Thick,
                    arrow_end: true,
                    arrow_start: false,
                },
                start + 3 + end_idx + 3,
            ));
        }
    }

    // Standard arrows (check longest tokens first)
    let patterns: &[(&str, EdgeStyle, bool, bool)] = &[
        ("<-->", EdgeStyle::Solid, true, true),
        ("<==>", EdgeStyle::Thick, true, true),
        ("<-.->", EdgeStyle::Dashed, true, true),
        ("-..->", EdgeStyle::Dashed, true, false),
        ("-.->", EdgeStyle::Dashed, true, false),
        ("-.-", EdgeStyle::Dashed, false, false),
        ("--->", EdgeStyle::Solid, true, false),
        ("-->", EdgeStyle::Solid, true, false),
        ("---", EdgeStyle::Solid, false, false),
        ("===>", EdgeStyle::Thick, true, false),
        ("==>", EdgeStyle::Thick, true, false),
        ("===", EdgeStyle::Thick, false, false),
        ("~~~", EdgeStyle::Dashed, false, false),
    ];

    for &(pat, style, arrow_end, arrow_start) in patterns {
        if let Some(after_pat) = rest.strip_prefix(pat) {
            let mut end = start + pat.len();
            let mut label = None;
            let trimmed_after = after_pat.trim_start();
            let ws_len = after_pat.len() - trimmed_after.len();
            if let Some(after_bar) = trimmed_after.strip_prefix('|') {
                if let Some(close_bar) = after_bar.find('|') {
                    let lbl = clean_diagram_label(&after_bar[..close_bar]);
                    if !lbl.is_empty() {
                        label = Some(lbl);
                    }
                    end = start + pat.len() + ws_len + 1 + close_bar + 1;
                }
            }
            return Some((
                EdgeSpec {
                    label,
                    style,
                    arrow_end,
                    arrow_start,
                },
                end,
            ));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// 2. Sequence Diagram parser
// ---------------------------------------------------------------------------

pub(super) fn parse_sequence_diagram(lines: &[&str]) -> SequenceDiagram {
    let mut participants: Vec<SeqParticipant> = Vec::new();
    let mut id_map: HashMap<String, usize> = HashMap::new();
    let mut events: Vec<SeqEvent> = Vec::new();

    let mut ensure_participant = |id: &str, label: Option<&str>, is_actor: bool| -> usize {
        let key = id.trim();
        if let Some(&idx) = id_map.get(key) {
            if let Some(lbl) = label {
                participants[idx].label = lbl.trim().to_owned();
            }
            idx
        } else {
            let idx = participants.len();
            participants.push(SeqParticipant {
                id: key.to_owned(),
                label: label.unwrap_or(key).trim().to_owned(),
                is_actor,
            });
            id_map.insert(key.to_owned(), idx);
            idx
        }
    };

    for &line in lines.iter().skip(1) {
        let lower = line.to_ascii_lowercase();
        if lower == "autonumber"
            || lower.starts_with("activate ")
            || lower.starts_with("deactivate ")
            || lower.starts_with("title ")
        {
            continue;
        }
        if let Some(rest) = line
            .strip_prefix("participant ")
            .or_else(|| line.strip_prefix("Participant "))
        {
            if let Some((id, alias)) = rest.split_once(" as ") {
                ensure_participant(id, Some(alias), false);
            } else {
                ensure_participant(rest, None, false);
            }
            continue;
        }
        if let Some(rest) = line
            .strip_prefix("actor ")
            .or_else(|| line.strip_prefix("Actor "))
        {
            if let Some((id, alias)) = rest.split_once(" as ") {
                ensure_participant(id, Some(alias), true);
            } else {
                ensure_participant(rest, None, true);
            }
            continue;
        }
        if lower.starts_with("loop ")
            || lower.starts_with("alt ")
            || lower.starts_with("opt ")
            || lower.starts_with("par ")
            || lower.starts_with("critical ")
        {
            let mut parts = line.splitn(2, ' ');
            let kind = parts.next().unwrap_or("").to_owned();
            let label = parts.next().unwrap_or("").trim().to_owned();
            events.push(SeqEvent::BlockStart { kind, label });
            continue;
        }
        if lower.starts_with("else") {
            let label = line[4..].trim().to_owned();
            events.push(SeqEvent::BlockElse { label });
            continue;
        }
        if lower == "end" {
            events.push(SeqEvent::BlockEnd);
            continue;
        }
        if lower.starts_with("note ") {
            if let Some((head, body)) = line.split_once(':') {
                let head_rest = head[5..].trim();
                let target_spec = head_rest
                    .strip_prefix("over ")
                    .or_else(|| head_rest.strip_prefix("Over "))
                    .or_else(|| head_rest.strip_prefix("right of "))
                    .or_else(|| head_rest.strip_prefix("Right of "))
                    .or_else(|| head_rest.strip_prefix("left of "))
                    .or_else(|| head_rest.strip_prefix("Left of "))
                    .unwrap_or(head_rest);
                let targets: Vec<&str> = target_spec.split(',').map(str::trim).collect();
                if let Some(&first_t) = targets.first() {
                    let from = ensure_participant(first_t, None, false);
                    let to = targets
                        .get(1)
                        .map(|t| ensure_participant(t, None, false))
                        .unwrap_or(from);
                    events.push(SeqEvent::Note {
                        from,
                        to,
                        text: clean_diagram_label(body),
                    });
                }
            }
            continue;
        }

        // Message arrows: `-->>`, `->>`, `--x`, `-x`, `-->`, `->`
        let arrow_specs: &[(&str, bool, bool)] = &[
            ("-->>+", true, false),
            ("-->>-", true, false),
            ("-->>", true, false),
            ("->>+", false, false),
            ("->>-", false, false),
            ("->>", false, false),
            ("--x", true, true),
            ("-x", false, true),
            ("-->+", true, false),
            ("-->-", true, false),
            ("-->", true, false),
            ("->+", false, false),
            ("->-", false, false),
            ("->", false, false),
        ];
        for &(tok, dashed, cross) in arrow_specs {
            if let Some(pos) = line.find(tok) {
                let lhs = line[..pos].trim();
                let rhs_full = &line[pos + tok.len()..];
                let (rhs, msg) = if let Some((r, m)) = rhs_full.split_once(':') {
                    (r.trim(), clean_diagram_label(m))
                } else {
                    (rhs_full.trim(), String::new())
                };
                if !lhs.is_empty() && !rhs.is_empty() {
                    let from = ensure_participant(lhs, None, false);
                    let to = ensure_participant(rhs, None, false);
                    events.push(SeqEvent::Message {
                        from,
                        to,
                        text: msg,
                        dashed,
                        cross,
                    });
                }
                break;
            }
        }
    }

    SequenceDiagram {
        participants,
        events,
    }
}

// ---------------------------------------------------------------------------
// 3. Pie Chart & XY Chart parsers
// ---------------------------------------------------------------------------

pub(super) fn parse_pie_chart(lines: &[&str]) -> PieChart {
    let mut title = None;
    let mut slices = Vec::new();

    if let Some(first) = lines.first() {
        let lower = first.to_ascii_lowercase();
        if let Some(idx) = lower.find("title ") {
            let t = first[idx + 6..].trim();
            if !t.is_empty() {
                title = Some(trim_quotes(t));
            }
        }
    }

    for &line in lines.iter().skip(1) {
        if let Some(rest) = line
            .strip_prefix("title ")
            .or_else(|| line.strip_prefix("Title "))
        {
            title = Some(trim_quotes(rest));
            continue;
        }
        if let Some((lhs, rhs)) = line.rsplit_once(':') {
            let label = trim_quotes(lhs);
            if let Ok(val) = rhs.trim().parse::<f32>() {
                if val > 0.0 && !label.is_empty() {
                    slices.push(PieSlice { label, value: val });
                }
            }
        }
    }

    PieChart { title, slices }
}

pub(super) fn parse_xy_chart(lines: &[&str]) -> XyChart {
    let mut title = None;
    let mut x_labels = Vec::new();
    let mut y_label = None;
    let mut y_min = None;
    let mut y_max = None;
    let mut bars = Vec::new();
    let mut lines_series = Vec::new();

    for &line in lines.iter().skip(1) {
        let lower = line.to_ascii_lowercase();
        if lower.starts_with("title ") {
            title = Some(trim_quotes(&line[6..]));
        } else if lower.starts_with("x-axis") {
            if let (Some(open), Some(close)) = (line.find('['), line.rfind(']')) {
                if close > open {
                    x_labels = line[open + 1..close]
                        .split(',')
                        .map(|s| trim_quotes(s.trim()))
                        .filter(|s| !s.is_empty())
                        .collect();
                }
            }
        } else if lower.starts_with("y-axis") {
            let rest = line[6..].trim();
            // Extract optional quoted label
            if let Some(q1) = rest.find('"') {
                if let Some(q2) = rest[q1 + 1..].find('"') {
                    y_label = Some(rest[q1 + 1..q1 + 1 + q2].to_owned());
                }
            }
            if let Some((a, b)) = rest.split_once("-->") {
                let min_tok = a.split_whitespace().last().unwrap_or("");
                let max_tok = b.split_whitespace().next().unwrap_or("");
                if let (Ok(lo), Ok(hi)) = (min_tok.parse::<f32>(), max_tok.parse::<f32>()) {
                    y_min = Some(lo);
                    y_max = Some(hi);
                }
            }
        } else if lower.starts_with("bar") {
            let nums = parse_number_bracket_list(line);
            if !nums.is_empty() {
                bars.push(nums);
            }
        } else if lower.starts_with("line") {
            let nums = parse_number_bracket_list(line);
            if !nums.is_empty() {
                lines_series.push(nums);
            }
        }
    }

    XyChart {
        title,
        x_labels,
        y_label,
        y_min,
        y_max,
        bars,
        lines: lines_series,
    }
}

fn parse_number_bracket_list(line: &str) -> Vec<f32> {
    let Some(open) = line.find('[') else {
        return Vec::new();
    };
    let Some(close) = line.rfind(']') else {
        return Vec::new();
    };
    if close <= open {
        return Vec::new();
    }
    line[open + 1..close]
        .split(',')
        .filter_map(|s| s.trim().parse::<f32>().ok())
        .collect()
}
