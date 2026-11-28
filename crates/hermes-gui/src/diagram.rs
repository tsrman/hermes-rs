//! Native Mermaid diagram parser, layout engine, and GPUI vector canvas renderer.
//! Supports flowcharts (`flowchart`/`graph`), state diagrams (`stateDiagram`), ER diagrams
//! (`erDiagram`), sequence diagrams (`sequenceDiagram`), pie charts (`pie`), and XY bar/line
//! charts (`xychart-beta`/`xychart`).

use std::collections::HashMap;
use std::f32::consts::PI;

use gpui::{
    canvas, font, point, prelude::*, px, quad, rgb, size, AnyElement, App, BorderStyle, Bounds,
    Corners, Edges, FontWeight, Hsla, PathBuilder, Pixels, Point, SharedString, TextRun, Window,
};

#[derive(Clone, Debug, PartialEq)]
pub enum Diagram {
    Flow(FlowGraph),
    Sequence(SequenceDiagram),
    Pie(PieChart),
    Xy(XyChart),
}

impl Diagram {
    pub fn kind_label(&self) -> &'static str {
        match self {
            Self::Flow(g) => g.kind_label,
            Self::Sequence(_) => "Sequence",
            Self::Pie(_) => "Pie Chart",
            Self::Xy(_) => "XY Chart",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FlowDirection {
    TopDown,
    BottomUp,
    LeftRight,
    RightLeft,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NodeShape {
    Rect,
    Rounded,
    Stadium,
    Diamond,
    Circle,
    Database,
    Subroutine,
    StartDot,
    EndDot,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EdgeStyle {
    Solid,
    Dashed,
    Thick,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FlowNode {
    pub id: String,
    pub label: String,
    pub shape: NodeShape,
    pub subgraph: Option<usize>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FlowEdge {
    pub from: usize,
    pub to: usize,
    pub label: Option<String>,
    pub style: EdgeStyle,
    pub arrow_end: bool,
    pub arrow_start: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FlowSubgraph {
    pub title: String,
    pub nodes: Vec<usize>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FlowGraph {
    pub kind_label: &'static str,
    pub direction: FlowDirection,
    pub nodes: Vec<FlowNode>,
    pub edges: Vec<FlowEdge>,
    pub subgraphs: Vec<FlowSubgraph>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SeqParticipant {
    pub id: String,
    pub label: String,
    pub is_actor: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum SeqEvent {
    Message {
        from: usize,
        to: usize,
        text: String,
        dashed: bool,
        cross: bool,
    },
    Note {
        from: usize,
        to: usize,
        text: String,
    },
    BlockStart {
        kind: String,
        label: String,
    },
    BlockElse {
        label: String,
    },
    BlockEnd,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SequenceDiagram {
    pub participants: Vec<SeqParticipant>,
    pub events: Vec<SeqEvent>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PieSlice {
    pub label: String,
    pub value: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PieChart {
    pub title: Option<String>,
    pub slices: Vec<PieSlice>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct XyChart {
    pub title: Option<String>,
    pub x_labels: Vec<String>,
    pub y_label: Option<String>,
    pub y_min: Option<f32>,
    pub y_max: Option<f32>,
    pub bars: Vec<Vec<f32>>,
    pub lines: Vec<Vec<f32>>,
}

/// Parse a Mermaid code block into a renderable `Diagram`. Returns `None` if the diagram type is
/// unrecognized or has no renderable items yet (e.g., mid-stream).
pub fn parse_mermaid(code: &str) -> Option<Diagram> {
    let lines: Vec<&str> = code
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("%%"))
        .collect();
    let first = *lines.first()?;
    let lower = first.to_ascii_lowercase();

    if lower.starts_with("flowchart") || lower.starts_with("graph") {
        let g = parse_flowchart(&lines, "Flowchart");
        return (!g.nodes.is_empty()).then_some(Diagram::Flow(g));
    }
    if lower.starts_with("statediagram") {
        let g = parse_state_diagram(&lines);
        return (!g.nodes.is_empty()).then_some(Diagram::Flow(g));
    }
    if lower.starts_with("erdiagram") {
        let g = parse_er_diagram(&lines);
        return (!g.nodes.is_empty()).then_some(Diagram::Flow(g));
    }
    if lower.starts_with("sequencediagram") {
        let seq = parse_sequence_diagram(&lines);
        return (!seq.participants.is_empty()).then_some(Diagram::Sequence(seq));
    }
    if lower == "pie" || lower.starts_with("pie ") {
        let pie = parse_pie_chart(&lines);
        return (!pie.slices.is_empty()).then_some(Diagram::Pie(pie));
    }
    if lower.starts_with("xychart") {
        let xy = parse_xy_chart(&lines);
        return (!xy.bars.is_empty() || !xy.lines.is_empty()).then_some(Diagram::Xy(xy));
    }
    None
}

/// Render a parsed `Diagram` as a responsive GPUI vector canvas element.
pub fn render_diagram(diagram: Diagram) -> AnyElement {
    match diagram {
        Diagram::Flow(graph) => render_flow_canvas(graph),
        Diagram::Sequence(seq) => render_sequence_canvas(seq),
        Diagram::Pie(pie) => render_pie_canvas(pie),
        Diagram::Xy(xy) => render_xy_canvas(xy),
    }
}

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

fn parse_flowchart(lines: &[&str], kind_label: &'static str) -> FlowGraph {
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

fn parse_state_diagram(lines: &[&str]) -> FlowGraph {
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

fn parse_er_diagram(lines: &[&str]) -> FlowGraph {
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

fn parse_sequence_diagram(lines: &[&str]) -> SequenceDiagram {
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

fn parse_pie_chart(lines: &[&str]) -> PieChart {
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

fn parse_xy_chart(lines: &[&str]) -> XyChart {
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

// ---------------------------------------------------------------------------
// 4. Vector Canvas Painting Helpers
// ---------------------------------------------------------------------------

fn paint_text_centered(
    window: &mut Window,
    cx: &mut App,
    center: Point<Pixels>,
    text: &str,
    font_size: f32,
    color: Hsla,
    bold: bool,
) {
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return;
    }
    let line_h = font_size * 1.28;
    let total_h = line_h * lines.len() as f32;
    let mut f = font(if cfg!(target_os = "windows") {
        "Segoe UI"
    } else {
        ".SystemUIFont"
    });
    if bold {
        f.weight = FontWeight::SEMIBOLD;
    }

    for (i, line_str) in lines.iter().enumerate() {
        if line_str.is_empty() {
            continue;
        }
        let run = TextRun {
            len: line_str.len(),
            font: f.clone(),
            color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let shaped = window.text_system().shape_line(
            SharedString::from((*line_str).to_owned()),
            px(font_size),
            &[run],
            None,
        );
        let x = center.x - shaped.width * 0.5;
        let y = center.y - px(total_h * 0.5) + px(i as f32 * line_h);
        let _ = shaped.paint(point(x, y), px(line_h), window, cx);
    }
}

fn paint_text_left(
    window: &mut Window,
    cx: &mut App,
    origin: Point<Pixels>,
    text: &str,
    font_size: f32,
    color: Hsla,
    bold: bool,
) {
    if text.is_empty() {
        return;
    }
    let mut f = font(if cfg!(target_os = "windows") {
        "Segoe UI"
    } else {
        ".SystemUIFont"
    });
    if bold {
        f.weight = FontWeight::SEMIBOLD;
    }
    let run = TextRun {
        len: text.len(),
        font: f,
        color,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let shaped = window.text_system().shape_line(
        SharedString::from(text.to_owned()),
        px(font_size),
        &[run],
        None,
    );
    let _ = shaped.paint(origin, px(font_size * 1.3), window, cx);
}

fn paint_arrowhead(
    window: &mut Window,
    tip: Point<Pixels>,
    dir_x: f32,
    dir_y: f32,
    size_px: f32,
    color: Hsla,
) {
    let len = (dir_x * dir_x + dir_y * dir_y).sqrt().max(0.001);
    let ux = dir_x / len;
    let uy = dir_y / len;
    let px_v = -uy;
    let py_v = ux;

    let back_x = f32::from(tip.x) - ux * size_px;
    let back_y = f32::from(tip.y) - uy * size_px;
    let half_w = size_px * 0.52;

    let mut pb = PathBuilder::fill();
    pb.add_polygon(
        &[
            tip,
            point(px(back_x + px_v * half_w), px(back_y + py_v * half_w)),
            point(px(back_x - px_v * half_w), px(back_y - py_v * half_w)),
        ],
        true,
    );
    if let Ok(path) = pb.build() {
        window.paint_path(path, color);
    }
}

fn paint_circle(
    window: &mut Window,
    cx_p: f32,
    cy_p: f32,
    r: f32,
    fill_color: Hsla,
    stroke_color: Option<(Hsla, f32)>,
) {
    let segs = 28;
    let pts: Vec<Point<Pixels>> = (0..segs)
        .map(|i| {
            let a = (i as f32 / segs as f32) * 2.0 * PI;
            point(px(cx_p + r * a.cos()), px(cy_p + r * a.sin()))
        })
        .collect();
    let mut pb = PathBuilder::fill();
    pb.add_polygon(&pts, true);
    if let Ok(path) = pb.build() {
        window.paint_path(path, fill_color);
    }
    if let Some((sc, sw)) = stroke_color {
        let mut sb = PathBuilder::stroke(px(sw));
        sb.add_polygon(&pts, true);
        if let Ok(path) = sb.build() {
            window.paint_path(path, sc);
        }
    }
}

// ---------------------------------------------------------------------------
// 5. Flowchart Layout & Renderer
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct LaidOutNode {
    cx: f32,
    cy: f32,
    w: f32,
    h: f32,
}

fn compute_flow_layout(graph: &FlowGraph) -> (Vec<LaidOutNode>, f32, f32) {
    let n = graph.nodes.len();
    if n == 0 {
        return (Vec::new(), 100.0, 60.0);
    }

    // Build adjacency list & detect cycle back-edges via DFS so rank assignment always terminates.
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (ei, e) in graph.edges.iter().enumerate() {
        if e.from != e.to {
            adj[e.from].push(ei);
        }
    }
    let mut state = vec![0u8; n]; // 0=unvisited, 1=visiting, 2=done
    let mut is_back_edge = vec![false; graph.edges.len()];
    fn dfs_cycle(
        u: usize,
        adj: &[Vec<usize>],
        edges: &[FlowEdge],
        state: &mut [u8],
        back: &mut [bool],
    ) {
        state[u] = 1;
        for &ei in &adj[u] {
            let v = edges[ei].to;
            if state[v] == 1 {
                back[ei] = true;
            } else if state[v] == 0 {
                dfs_cycle(v, adj, edges, state, back);
            }
        }
        state[u] = 2;
    }
    for u in 0..n {
        if state[u] == 0 {
            dfs_cycle(u, &adj, &graph.edges, &mut state, &mut is_back_edge);
        }
    }

    // Longest-path rank assignment on DAG edges.
    let mut rank = vec![0usize; n];
    for _ in 0..n {
        let mut changed = false;
        for (ei, e) in graph.edges.iter().enumerate() {
            if e.from == e.to || is_back_edge[ei] {
                continue;
            }
            if rank[e.to] <= rank[e.from] {
                rank[e.to] = rank[e.from] + 1;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    let max_rank = rank.iter().copied().max().unwrap_or(0);
    let mut layers: Vec<Vec<usize>> = vec![Vec::new(); max_rank + 1];
    for (idx, &r) in rank.iter().enumerate() {
        layers[r].push(idx);
    }

    // Measure each node's preferred dimensions.
    let dims: Vec<(f32, f32)> = graph
        .nodes
        .iter()
        .map(|node| match node.shape {
            NodeShape::StartDot => (22.0, 22.0),
            NodeShape::EndDot => (26.0, 26.0),
            _ => {
                let max_chars = node
                    .label
                    .lines()
                    .map(|l| l.chars().count())
                    .max()
                    .unwrap_or(4);
                let line_count = node.label.lines().count().max(1);
                let base_w = (max_chars as f32 * 7.6 + 30.0).clamp(68.0, 220.0);
                let base_h = (line_count as f32 * 17.0 + 20.0).max(38.0);
                match node.shape {
                    NodeShape::Diamond => (base_w + 26.0, base_h + 22.0),
                    NodeShape::Circle => {
                        let d = base_w.max(base_h).clamp(48.0, 110.0);
                        (d, d)
                    }
                    NodeShape::Database => (base_w + 10.0, base_h + 12.0),
                    _ => (base_w, base_h),
                }
            }
        })
        .collect();

    let horizontal = matches!(
        graph.direction,
        FlowDirection::LeftRight | FlowDirection::RightLeft
    );
    let reversed = matches!(
        graph.direction,
        FlowDirection::BottomUp | FlowDirection::RightLeft
    );

    if reversed {
        layers.reverse();
    }

    let pad_x = 34.0;
    let pad_y = 28.0;
    let cross_gap = 28.0;
    let main_gap = if horizontal { 64.0 } else { 54.0 };

    let mut placed = vec![
        LaidOutNode {
            cx: 0.0,
            cy: 0.0,
            w: 0.0,
            h: 0.0,
        };
        n
    ];

    if !horizontal {
        let layer_widths: Vec<f32> = layers
            .iter()
            .map(|layer| {
                layer.iter().map(|&idx| dims[idx].0).sum::<f32>()
                    + cross_gap * layer.len().saturating_sub(1) as f32
            })
            .collect();
        let max_w = layer_widths.iter().copied().fold(0.0f32, f32::max);
        let total_w = max_w + pad_x * 2.0;

        let mut cur_y = pad_y;
        for (li, layer) in layers.iter().enumerate() {
            let layer_h = layer.iter().map(|&idx| dims[idx].1).fold(0.0f32, f32::max);
            let mut cur_x = (total_w - layer_widths[li]) * 0.5;
            for &idx in layer {
                let (w, h) = dims[idx];
                placed[idx] = LaidOutNode {
                    cx: cur_x + w * 0.5,
                    cy: cur_y + layer_h * 0.5,
                    w,
                    h,
                };
                cur_x += w + cross_gap;
            }
            cur_y += layer_h + main_gap;
        }
        let total_h = (cur_y - main_gap + pad_y).max(80.0);
        (placed, total_w.max(180.0), total_h)
    } else {
        let layer_heights: Vec<f32> = layers
            .iter()
            .map(|layer| {
                layer.iter().map(|&idx| dims[idx].1).sum::<f32>()
                    + cross_gap * layer.len().saturating_sub(1) as f32
            })
            .collect();
        let max_h = layer_heights.iter().copied().fold(0.0f32, f32::max);
        let total_h = max_h + pad_y * 2.0;

        let mut cur_x = pad_x;
        for (li, layer) in layers.iter().enumerate() {
            let layer_w = layer.iter().map(|&idx| dims[idx].0).fold(0.0f32, f32::max);
            let mut cur_y = (total_h - layer_heights[li]) * 0.5;
            for &idx in layer {
                let (w, h) = dims[idx];
                placed[idx] = LaidOutNode {
                    cx: cur_x + layer_w * 0.5,
                    cy: cur_y + h * 0.5,
                    w,
                    h,
                };
                cur_y += h + cross_gap;
            }
            cur_x += layer_w + main_gap;
        }
        let total_w = (cur_x - main_gap + pad_x).max(200.0);
        (placed, total_w, total_h.max(96.0))
    }
}

fn render_flow_canvas(graph: FlowGraph) -> AnyElement {
    let (placed, nat_w, nat_h) = compute_flow_layout(&graph);
    let desired_h = nat_h.clamp(100.0, 560.0);

    canvas(
        |_, _, _| (),
        move |bounds: Bounds<Pixels>, _, window: &mut Window, cx: &mut App| {
            let avail_w = f32::from(bounds.size.width).max(120.0);
            let avail_h = f32::from(bounds.size.height).max(80.0);
            let scale: f32 = (avail_w / nat_w).min(avail_h / nat_h).clamp(0.45, 1.0);
            let ox = f32::from(bounds.origin.x) + (avail_w - nat_w * scale) * 0.5;
            let oy = f32::from(bounds.origin.y) + (avail_h - nat_h * scale) * 0.5;
            let tx = |x: f32| ox + x * scale;
            let ty = |y: f32| oy + y * scale;

            let edge_color: Hsla = rgb(0x64748b).into();
            let edge_thick: Hsla = rgb(0x60a5fa).into();
            let node_fill: Hsla = rgb(0x1b263b).into();
            let node_border: Hsla = rgb(0x4ea1ff).into();
            let diamond_fill: Hsla = rgb(0x28203a).into();
            let diamond_border: Hsla = rgb(0xc084fc).into();
            let db_fill: Hsla = rgb(0x182b2e).into();
            let db_border: Hsla = rgb(0x2dd4bf).into();
            let text_col: Hsla = rgb(0xe6edf6).into();
            let label_bg: Hsla = rgb(0x0f172a).into();
            let label_fg: Hsla = rgb(0x93c5fd).into();
            let sg_fill: Hsla = Hsla {
                h: 0.6,
                s: 0.35,
                l: 0.14,
                a: 0.45,
            };
            let sg_border: Hsla = rgb(0x334155).into();

            // 1. Subgraph containers
            for sg in &graph.subgraphs {
                if sg.nodes.is_empty() {
                    continue;
                }
                let mut min_x = f32::INFINITY;
                let mut min_y = f32::INFINITY;
                let mut max_x = f32::NEG_INFINITY;
                let mut max_y = f32::NEG_INFINITY;
                for &ni in &sg.nodes {
                    if let Some(p) = placed.get(ni) {
                        min_x = min_x.min(p.cx - p.w * 0.5);
                        min_y = min_y.min(p.cy - p.h * 0.5);
                        max_x = max_x.max(p.cx + p.w * 0.5);
                        max_y = max_y.max(p.cy + p.h * 0.5);
                    }
                }
                if !min_x.is_finite() {
                    continue;
                }
                let pad = 14.0;
                let top_pad = 24.0;
                let sg_bounds = Bounds::new(
                    point(px(tx(min_x - pad)), px(ty(min_y - top_pad))),
                    size(
                        px((max_x - min_x + pad * 2.0) * scale),
                        px((max_y - min_y + pad + top_pad) * scale),
                    ),
                );
                window.paint_quad(quad(
                    sg_bounds,
                    Corners::all(px(8.0 * scale)),
                    sg_fill,
                    Edges::all(px(1.0)),
                    sg_border,
                    BorderStyle::Dashed,
                ));
                if !sg.title.is_empty() {
                    paint_text_left(
                        window,
                        cx,
                        point(px(tx(min_x - pad + 8.0)), px(ty(min_y - top_pad + 4.0))),
                        &sg.title,
                        (11.0 * scale).max(9.0),
                        rgb(0x94a3b8).into(),
                        true,
                    );
                }
            }

            // 2. Edges
            let horizontal = matches!(
                graph.direction,
                FlowDirection::LeftRight | FlowDirection::RightLeft
            );
            for e in &graph.edges {
                let (Some(a), Some(b)) = (placed.get(e.from), placed.get(e.to)) else {
                    continue;
                };
                let stroke_w = match e.style {
                    EdgeStyle::Thick => 2.5 * scale,
                    _ => 1.5 * scale,
                };
                let col = match e.style {
                    EdgeStyle::Thick => edge_thick,
                    _ => edge_color,
                };

                if e.from == e.to {
                    // Self-loop on right side of node
                    let rx = tx(a.cx + a.w * 0.5);
                    let ry = ty(a.cy);
                    let loop_w = 24.0 * scale;
                    let loop_h = 14.0 * scale;
                    let mut pb = PathBuilder::stroke(px(stroke_w));
                    if e.style == EdgeStyle::Dashed {
                        pb = pb.dash_array(&[px(5.0 * scale), px(4.0 * scale)]);
                    }
                    pb.move_to(point(px(rx), px(ry - loop_h * 0.5)));
                    pb.cubic_bezier_to(
                        point(px(rx), px(ry + loop_h * 0.5)),
                        point(px(rx + loop_w), px(ry - loop_h)),
                        point(px(rx + loop_w), px(ry + loop_h)),
                    );
                    if let Ok(path) = pb.build() {
                        window.paint_path(path, col);
                    }
                    if e.arrow_end {
                        paint_arrowhead(
                            window,
                            point(px(rx), px(ry + loop_h * 0.5)),
                            -1.0,
                            0.0,
                            7.0 * scale,
                            col,
                        );
                    }
                    continue;
                }

                // Determine start and end port points on node perimeters
                let (sx, sy, ex, ey, c1x, c1y, c2x, c2y, label_x, label_y) = if !horizontal {
                    let going_down = b.cy >= a.cy;
                    if going_down {
                        let s_x = tx(a.cx);
                        let s_y = ty(a.cy + a.h * 0.5);
                        let e_x = tx(b.cx);
                        let e_y = ty(b.cy - b.h * 0.5);
                        let mid_y = (s_y + e_y) * 0.5;
                        (
                            s_x,
                            s_y,
                            e_x,
                            e_y,
                            s_x,
                            mid_y,
                            e_x,
                            mid_y,
                            (s_x + e_x) * 0.5,
                            mid_y,
                        )
                    } else {
                        // Back-edge: curve gently around the right side
                        let s_x = tx(a.cx + a.w * 0.5);
                        let s_y = ty(a.cy);
                        let e_x = tx(b.cx + b.w * 0.5);
                        let e_y = ty(b.cy);
                        let bend_x = s_x.max(e_x) + 32.0 * scale;
                        (
                            s_x,
                            s_y,
                            e_x,
                            e_y,
                            bend_x,
                            s_y,
                            bend_x,
                            e_y,
                            bend_x - 6.0 * scale,
                            (s_y + e_y) * 0.5,
                        )
                    }
                } else {
                    let going_right = b.cx >= a.cx;
                    if going_right {
                        let s_x = tx(a.cx + a.w * 0.5);
                        let s_y = ty(a.cy);
                        let e_x = tx(b.cx - b.w * 0.5);
                        let e_y = ty(b.cy);
                        let mid_x = (s_x + e_x) * 0.5;
                        (
                            s_x,
                            s_y,
                            e_x,
                            e_y,
                            mid_x,
                            s_y,
                            mid_x,
                            e_y,
                            mid_x,
                            (s_y + e_y) * 0.5,
                        )
                    } else {
                        let s_x = tx(a.cx);
                        let s_y = ty(a.cy + a.h * 0.5);
                        let e_x = tx(b.cx);
                        let e_y = ty(b.cy + b.h * 0.5);
                        let bend_y = s_y.max(e_y) + 28.0 * scale;
                        (
                            s_x,
                            s_y,
                            e_x,
                            e_y,
                            s_x,
                            bend_y,
                            e_x,
                            bend_y,
                            (s_x + e_x) * 0.5,
                            bend_y - 6.0 * scale,
                        )
                    }
                };

                let mut pb = PathBuilder::stroke(px(stroke_w));
                if e.style == EdgeStyle::Dashed {
                    pb = pb.dash_array(&[px(5.0 * scale), px(4.0 * scale)]);
                }
                pb.move_to(point(px(sx), px(sy)));
                pb.cubic_bezier_to(
                    point(px(ex), px(ey)),
                    point(px(c1x), px(c1y)),
                    point(px(c2x), px(c2y)),
                );
                if let Ok(path) = pb.build() {
                    window.paint_path(path, col);
                }

                if e.arrow_end {
                    let dx = ex - c2x;
                    let dy = ey - c2y;
                    let (dx, dy) = if dx.abs() + dy.abs() < 0.5 {
                        (ex - sx, ey - sy)
                    } else {
                        (dx, dy)
                    };
                    paint_arrowhead(window, point(px(ex), px(ey)), dx, dy, 8.0 * scale, col);
                }
                if e.arrow_start {
                    let dx = sx - c1x;
                    let dy = sy - c1y;
                    let (dx, dy) = if dx.abs() + dy.abs() < 0.5 {
                        (sx - ex, sy - ey)
                    } else {
                        (dx, dy)
                    };
                    paint_arrowhead(window, point(px(sx), px(sy)), dx, dy, 8.0 * scale, col);
                }

                if let Some(lbl) = &e.label {
                    let lw = (lbl.chars().count() as f32 * 6.5 + 12.0) * scale;
                    let lh = 16.0 * scale;
                    window.paint_quad(quad(
                        Bounds::new(
                            point(px(label_x - lw * 0.5), px(label_y - lh * 0.5)),
                            size(px(lw), px(lh)),
                        ),
                        Corners::all(px(4.0 * scale)),
                        label_bg,
                        Edges::all(px(1.0)),
                        rgb(0x334155),
                        BorderStyle::Solid,
                    ));
                    paint_text_centered(
                        window,
                        cx,
                        point(px(label_x), px(label_y)),
                        lbl,
                        (10.5 * scale).max(8.5),
                        label_fg,
                        false,
                    );
                }
            }

            // 3. Nodes
            for (idx, node) in graph.nodes.iter().enumerate() {
                let p = &placed[idx];
                let cx_p = tx(p.cx);
                let cy_p = ty(p.cy);
                let w = p.w * scale;
                let h = p.h * scale;
                let rect = Bounds::new(
                    point(px(cx_p - w * 0.5), px(cy_p - h * 0.5)),
                    size(px(w), px(h)),
                );

                match node.shape {
                    NodeShape::StartDot => {
                        paint_circle(window, cx_p, cy_p, w * 0.45, node_border, None);
                    }
                    NodeShape::EndDot => {
                        paint_circle(
                            window,
                            cx_p,
                            cy_p,
                            w * 0.48,
                            rgb(0x10141c).into(),
                            Some((node_border, 2.0 * scale)),
                        );
                        paint_circle(window, cx_p, cy_p, w * 0.28, node_border, None);
                    }
                    NodeShape::Diamond => {
                        let pts = [
                            point(px(cx_p), px(cy_p - h * 0.5)),
                            point(px(cx_p + w * 0.5), px(cy_p)),
                            point(px(cx_p), px(cy_p + h * 0.5)),
                            point(px(cx_p - w * 0.5), px(cy_p)),
                        ];
                        let mut fb = PathBuilder::fill();
                        fb.add_polygon(&pts, true);
                        if let Ok(path) = fb.build() {
                            window.paint_path(path, diamond_fill);
                        }
                        let mut sb = PathBuilder::stroke(px(1.5 * scale));
                        sb.add_polygon(&pts, true);
                        if let Ok(path) = sb.build() {
                            window.paint_path(path, diamond_border);
                        }
                    }
                    NodeShape::Circle => {
                        paint_circle(
                            window,
                            cx_p,
                            cy_p,
                            w.min(h) * 0.5,
                            node_fill,
                            Some((node_border, 1.5 * scale)),
                        );
                    }
                    NodeShape::Database => {
                        window.paint_quad(quad(
                            rect,
                            Corners::all(px(10.0 * scale)),
                            db_fill,
                            Edges::all(px(1.5 * scale)),
                            db_border,
                            BorderStyle::Solid,
                        ));
                        // Top cylinder rim line
                        let mut rim = PathBuilder::stroke(px(1.0 * scale));
                        rim.move_to(point(px(cx_p - w * 0.46), px(cy_p - h * 0.28)));
                        rim.curve_to(
                            point(px(cx_p + w * 0.46), px(cy_p - h * 0.28)),
                            point(px(cx_p), px(cy_p - h * 0.12)),
                        );
                        if let Ok(path) = rim.build() {
                            window.paint_path(path, db_border);
                        }
                    }
                    NodeShape::Subroutine => {
                        window.paint_quad(quad(
                            rect,
                            Corners::all(px(4.0 * scale)),
                            node_fill,
                            Edges::all(px(1.5 * scale)),
                            node_border,
                            BorderStyle::Solid,
                        ));
                        for side in [-1.0f32, 1.0] {
                            let x = cx_p + side * (w * 0.5 - 7.0 * scale);
                            let mut sb = PathBuilder::stroke(px(1.0 * scale));
                            sb.move_to(point(px(x), px(cy_p - h * 0.5)));
                            sb.line_to(point(px(x), px(cy_p + h * 0.5)));
                            if let Ok(path) = sb.build() {
                                window.paint_path(path, node_border);
                            }
                        }
                    }
                    NodeShape::Stadium => {
                        window.paint_quad(quad(
                            rect,
                            Corners::all(px(h * 0.5)),
                            node_fill,
                            Edges::all(px(1.5 * scale)),
                            node_border,
                            BorderStyle::Solid,
                        ));
                    }
                    NodeShape::Rounded => {
                        window.paint_quad(quad(
                            rect,
                            Corners::all(px(10.0 * scale)),
                            node_fill,
                            Edges::all(px(1.5 * scale)),
                            node_border,
                            BorderStyle::Solid,
                        ));
                    }
                    NodeShape::Rect => {
                        window.paint_quad(quad(
                            rect,
                            Corners::all(px(4.0 * scale)),
                            node_fill,
                            Edges::all(px(1.5 * scale)),
                            node_border,
                            BorderStyle::Solid,
                        ));
                    }
                }

                if !node.label.is_empty() {
                    paint_text_centered(
                        window,
                        cx,
                        point(px(cx_p), px(cy_p)),
                        &node.label,
                        (12.0 * scale).max(9.0),
                        text_col,
                        false,
                    );
                }
            }
        },
    )
    .w_full()
    .h(px(desired_h))
    .into_any_element()
}

// ---------------------------------------------------------------------------
// 6. Sequence Diagram Renderer
// ---------------------------------------------------------------------------

fn render_sequence_canvas(seq: SequenceDiagram) -> AnyElement {
    let p_count = seq.participants.len().max(1);
    let col_w = 155.0f32;
    let pad_x = 32.0f32;
    let header_h = 34.0f32;
    let step_h = 40.0f32;
    let top_y = 18.0f32;

    let visible_steps = seq
        .events
        .iter()
        .filter(|e| matches!(e, SeqEvent::Message { .. } | SeqEvent::Note { .. }))
        .count()
        .max(1);

    let nat_w = (p_count as f32 * col_w + pad_x * 2.0).max(260.0);
    let nat_h = top_y + header_h + 24.0 + visible_steps as f32 * step_h + header_h + 24.0;
    let desired_h = nat_h.clamp(140.0, 560.0);

    canvas(
        |_, _, _| (),
        move |bounds: Bounds<Pixels>, _, window: &mut Window, cx: &mut App| {
            let avail_w = f32::from(bounds.size.width).max(180.0);
            let avail_h = f32::from(bounds.size.height).max(100.0);
            let scale: f32 = (avail_w / nat_w).min(avail_h / nat_h).clamp(0.5, 1.0);
            let ox = f32::from(bounds.origin.x) + (avail_w - nat_w * scale) * 0.5;
            let oy = f32::from(bounds.origin.y) + (avail_h - nat_h * scale) * 0.5;
            let tx = |x: f32| ox + x * scale;
            let ty = |y: f32| oy + y * scale;

            let box_fill: Hsla = rgb(0x1b263b).into();
            let box_border: Hsla = rgb(0x4ea1ff).into();
            let lifeline_col: Hsla = rgb(0x334155).into();
            let arrow_col: Hsla = rgb(0x93c5fd).into();
            let text_col: Hsla = rgb(0xe6edf6).into();
            let note_fill: Hsla = rgb(0x2d2616).into();
            let note_border: Hsla = rgb(0xd29922).into();
            let frame_border: Hsla = rgb(0x475569).into();

            let col_center = |i: usize| pad_x + col_w * (i as f32 + 0.5);
            let lifeline_top = top_y + header_h;
            let lifeline_bot = nat_h - header_h - 18.0;

            // Vertical lifelines
            for i in 0..seq.participants.len() {
                let x = tx(col_center(i));
                let mut pb = PathBuilder::stroke(px(1.2 * scale))
                    .dash_array(&[px(5.0 * scale), px(4.0 * scale)]);
                pb.move_to(point(px(x), px(ty(lifeline_top))));
                pb.line_to(point(px(x), px(ty(lifeline_bot))));
                if let Ok(path) = pb.build() {
                    window.paint_path(path, lifeline_col);
                }
            }

            // Pre-pass for frame boxes (`loop`/`alt`/`opt`)
            struct SeqFrame {
                kind: String,
                label: String,
                y0: f32,
                y1: f32,
                elses: Vec<(f32, String)>,
            }
            let mut open_frames: Vec<SeqFrame> = Vec::new();
            let mut completed_frames: Vec<SeqFrame> = Vec::new();
            let mut sim_y = lifeline_top + 26.0;
            for ev in &seq.events {
                match ev {
                    SeqEvent::BlockStart { kind, label } => {
                        open_frames.push(SeqFrame {
                            kind: kind.clone(),
                            label: label.clone(),
                            y0: sim_y - 16.0,
                            y1: sim_y - 16.0,
                            elses: Vec::new(),
                        });
                        sim_y += 14.0;
                    }
                    SeqEvent::BlockElse { label } => {
                        if let Some(top) = open_frames.last_mut() {
                            top.elses.push((sim_y - 8.0, label.clone()));
                        }
                        sim_y += 12.0;
                    }
                    SeqEvent::BlockEnd => {
                        if let Some(mut frame) = open_frames.pop() {
                            frame.y1 = sim_y - 6.0;
                            completed_frames.push(frame);
                        }
                    }
                    SeqEvent::Message { .. } | SeqEvent::Note { .. } => {
                        sim_y += step_h;
                    }
                }
            }

            // Draw frames behind messages
            for SeqFrame {
                kind,
                label,
                y0,
                y1,
                elses,
            } in completed_frames
            {
                let f_bounds = Bounds::new(
                    point(px(tx(pad_x * 0.5)), px(ty(y0))),
                    size(px((nat_w - pad_x) * scale), px((y1 - y0).max(24.0) * scale)),
                );
                window.paint_quad(quad(
                    f_bounds,
                    Corners::all(px(5.0 * scale)),
                    Hsla {
                        h: 0.6,
                        s: 0.25,
                        l: 0.14,
                        a: 0.35,
                    },
                    Edges::all(px(1.0)),
                    frame_border,
                    BorderStyle::Solid,
                ));
                let badge = if label.is_empty() {
                    kind
                } else {
                    format!("{kind} [{label}]")
                };
                paint_text_left(
                    window,
                    cx,
                    point(px(tx(pad_x * 0.5 + 6.0)), px(ty(y0 + 3.0))),
                    &badge,
                    (10.0 * scale).max(8.5),
                    rgb(0x94a3b8).into(),
                    true,
                );
                for (ey, el_lbl) in elses {
                    let mut pb = PathBuilder::stroke(px(1.0 * scale))
                        .dash_array(&[px(4.0 * scale), px(4.0 * scale)]);
                    pb.move_to(point(px(tx(pad_x * 0.5)), px(ty(ey))));
                    pb.line_to(point(px(tx(nat_w - pad_x * 0.5)), px(ty(ey))));
                    if let Ok(path) = pb.build() {
                        window.paint_path(path, frame_border);
                    }
                    if !el_lbl.is_empty() {
                        paint_text_left(
                            window,
                            cx,
                            point(px(tx(pad_x * 0.5 + 6.0)), px(ty(ey + 2.0))),
                            &format!("else [{el_lbl}]"),
                            (9.5 * scale).max(8.0),
                            rgb(0x94a3b8).into(),
                            false,
                        );
                    }
                }
            }

            // Draw events (messages and notes)
            let mut cur_y = lifeline_top + 26.0;
            for ev in &seq.events {
                match ev {
                    SeqEvent::BlockStart { .. } => cur_y += 14.0,
                    SeqEvent::BlockElse { .. } => cur_y += 12.0,
                    SeqEvent::BlockEnd => {}
                    SeqEvent::Note { from, to, text } => {
                        let x0 = col_center(*from.min(to)) - 54.0;
                        let x1 = col_center(*from.max(to)) + 54.0;
                        let nb = Bounds::new(
                            point(px(tx(x0)), px(ty(cur_y - 13.0))),
                            size(px((x1 - x0) * scale), px(26.0 * scale)),
                        );
                        window.paint_quad(quad(
                            nb,
                            Corners::all(px(4.0 * scale)),
                            note_fill,
                            Edges::all(px(1.0)),
                            note_border,
                            BorderStyle::Solid,
                        ));
                        paint_text_centered(
                            window,
                            cx,
                            point(px(tx((x0 + x1) * 0.5)), px(ty(cur_y))),
                            text,
                            (10.5 * scale).max(8.5),
                            rgb(0xfde68a).into(),
                            false,
                        );
                        cur_y += step_h;
                    }
                    SeqEvent::Message {
                        from,
                        to,
                        text,
                        dashed,
                        cross,
                    } => {
                        let x_from = tx(col_center(*from));
                        let x_to = tx(col_center(*to));
                        let y = ty(cur_y);

                        if from == to {
                            // Self-message loop
                            let loop_w = 34.0 * scale;
                            let loop_h = 18.0 * scale;
                            let mut pb = PathBuilder::stroke(px(1.5 * scale));
                            if *dashed {
                                pb = pb.dash_array(&[px(5.0 * scale), px(4.0 * scale)]);
                            }
                            pb.move_to(point(px(x_from), px(y - 6.0 * scale)));
                            pb.line_to(point(px(x_from + loop_w), px(y - 6.0 * scale)));
                            pb.line_to(point(px(x_from + loop_w), px(y + loop_h - 6.0 * scale)));
                            pb.line_to(point(px(x_from), px(y + loop_h - 6.0 * scale)));
                            if let Ok(path) = pb.build() {
                                window.paint_path(path, arrow_col);
                            }
                            paint_arrowhead(
                                window,
                                point(px(x_from), px(y + loop_h - 6.0 * scale)),
                                -1.0,
                                0.0,
                                7.0 * scale,
                                arrow_col,
                            );
                            if !text.is_empty() {
                                paint_text_left(
                                    window,
                                    cx,
                                    point(px(x_from + loop_w + 6.0 * scale), px(y - 8.0 * scale)),
                                    text,
                                    (11.0 * scale).max(8.5),
                                    text_col,
                                    false,
                                );
                            }
                        } else {
                            let mut pb = PathBuilder::stroke(px(1.5 * scale));
                            if *dashed {
                                pb = pb.dash_array(&[px(5.0 * scale), px(4.0 * scale)]);
                            }
                            pb.move_to(point(px(x_from), px(y)));
                            pb.line_to(point(px(x_to), px(y)));
                            if let Ok(path) = pb.build() {
                                window.paint_path(path, arrow_col);
                            }
                            if *cross {
                                let s = 5.0 * scale;
                                let mut xb = PathBuilder::stroke(px(1.8 * scale));
                                xb.move_to(point(px(x_to - s), px(y - s)));
                                xb.line_to(point(px(x_to + s), px(y + s)));
                                xb.move_to(point(px(x_to - s), px(y + s)));
                                xb.line_to(point(px(x_to + s), px(y - s)));
                                if let Ok(path) = xb.build() {
                                    window.paint_path(path, rgb(0xf85149));
                                }
                            } else {
                                let dir = if x_to > x_from { 1.0 } else { -1.0 };
                                paint_arrowhead(
                                    window,
                                    point(px(x_to), px(y)),
                                    dir,
                                    0.0,
                                    8.0 * scale,
                                    arrow_col,
                                );
                            }
                            if !text.is_empty() {
                                paint_text_centered(
                                    window,
                                    cx,
                                    point(px((x_from + x_to) * 0.5), px(y - 11.0 * scale)),
                                    text,
                                    (11.0 * scale).max(8.5),
                                    text_col,
                                    false,
                                );
                            }
                        }
                        cur_y += step_h;
                    }
                }
            }

            // Participant header & footer boxes
            for (i, p) in seq.participants.iter().enumerate() {
                let cx_p = col_center(i);
                let bw = (p.label.chars().count() as f32 * 7.6 + 26.0).clamp(72.0, col_w - 16.0);
                for &by in &[top_y, lifeline_bot] {
                    let rect = Bounds::new(
                        point(px(tx(cx_p - bw * 0.5)), px(ty(by))),
                        size(px(bw * scale), px(header_h * scale)),
                    );
                    let radius = if p.is_actor { header_h * 0.5 } else { 6.0 };
                    window.paint_quad(quad(
                        rect,
                        Corners::all(px(radius * scale)),
                        box_fill,
                        Edges::all(px(1.5 * scale)),
                        box_border,
                        BorderStyle::Solid,
                    ));
                    paint_text_centered(
                        window,
                        cx,
                        point(px(tx(cx_p)), px(ty(by + header_h * 0.5))),
                        &p.label,
                        (12.0 * scale).max(9.0),
                        text_col,
                        true,
                    );
                }
            }
        },
    )
    .w_full()
    .h(px(desired_h))
    .into_any_element()
}

// ---------------------------------------------------------------------------
// 7. Pie Chart Renderer
// ---------------------------------------------------------------------------

const CHART_PALETTE: [u32; 8] = [
    0x4ea1ff, 0x3fb950, 0xf59e0b, 0xc084fc, 0xf43f5e, 0x2dd4bf, 0xfb923c, 0x818cf8,
];

fn render_pie_canvas(pie: PieChart) -> AnyElement {
    let legend_rows = pie.slices.len().max(1);
    let title_h = if pie.title.is_some() { 28.0 } else { 8.0 };
    let desired_h = (title_h + (legend_rows as f32 * 24.0).max(176.0) + 24.0).clamp(190.0, 360.0);

    canvas(
        |_, _, _| (),
        move |bounds: Bounds<Pixels>, _, window: &mut Window, cx: &mut App| {
            let w = f32::from(bounds.size.width).max(240.0);
            let h = f32::from(bounds.size.height).max(160.0);
            let ox = f32::from(bounds.origin.x);
            let oy = f32::from(bounds.origin.y);

            if let Some(title) = &pie.title {
                paint_text_centered(
                    window,
                    cx,
                    point(px(ox + w * 0.5), px(oy + 16.0)),
                    title,
                    13.0,
                    rgb(0xe6edf6).into(),
                    true,
                );
            }

            let total: f32 = pie.slices.iter().map(|s| s.value).sum::<f32>().max(0.0001);
            let r = ((h - title_h - 24.0) * 0.46)
                .min(w * 0.23)
                .clamp(44.0, 92.0);
            let pie_cx = ox + (w * 0.30).max(r + 24.0);
            let pie_cy = oy + title_h + (h - title_h) * 0.5;

            let mut angle = -PI * 0.5;
            for (idx, slice) in pie.slices.iter().enumerate() {
                let frac = slice.value / total;
                let sweep = frac * 2.0 * PI;
                let col: Hsla = rgb(CHART_PALETTE[idx % CHART_PALETTE.len()]).into();

                let steps = ((sweep / (PI / 24.0)).ceil() as usize).max(3);
                let mut pts = Vec::with_capacity(steps + 2);
                pts.push(point(px(pie_cx), px(pie_cy)));
                for step in 0..=steps {
                    let a = angle + sweep * (step as f32 / steps as f32);
                    pts.push(point(px(pie_cx + r * a.cos()), px(pie_cy + r * a.sin())));
                }
                let mut fb = PathBuilder::fill();
                fb.add_polygon(&pts, true);
                if let Ok(path) = fb.build() {
                    window.paint_path(path, col);
                }
                // Crisp divider stroke
                let mut sb = PathBuilder::stroke(px(1.2));
                sb.move_to(point(px(pie_cx), px(pie_cy)));
                sb.line_to(point(
                    px(pie_cx + r * angle.cos()),
                    px(pie_cy + r * angle.sin()),
                ));
                if let Ok(path) = sb.build() {
                    window.paint_path(path, rgb(0x10141c));
                }

                // Percentage inside large slices
                if frac >= 0.08 {
                    let mid = angle + sweep * 0.5;
                    let lx = pie_cx + r * 0.62 * mid.cos();
                    let ly = pie_cy + r * 0.62 * mid.sin();
                    paint_text_centered(
                        window,
                        cx,
                        point(px(lx), px(ly)),
                        &format!("{:.0}%", frac * 100.0),
                        10.5,
                        rgb(0x0b1017).into(),
                        true,
                    );
                }

                angle += sweep;
            }

            // Legend on the right
            let leg_x = pie_cx + r + 32.0;
            let leg_total_h = pie.slices.len() as f32 * 22.0;
            let leg_start_y = pie_cy - leg_total_h * 0.5;
            for (idx, slice) in pie.slices.iter().enumerate() {
                let y = leg_start_y + idx as f32 * 22.0;
                let col: Hsla = rgb(CHART_PALETTE[idx % CHART_PALETTE.len()]).into();
                window.paint_quad(quad(
                    Bounds::new(point(px(leg_x), px(y + 3.0)), size(px(11.0), px(11.0))),
                    Corners::all(px(3.0)),
                    col,
                    Edges::all(px(0.0)),
                    col,
                    BorderStyle::Solid,
                ));
                let pct = (slice.value / total) * 100.0;
                let val_str = if (slice.value - slice.value.round()).abs() < 0.01 {
                    format!("{:.0}", slice.value)
                } else {
                    format!("{:.1}", slice.value)
                };
                paint_text_left(
                    window,
                    cx,
                    point(px(leg_x + 18.0), px(y)),
                    &format!("{} — {} ({:.1}%)", slice.label, val_str, pct),
                    11.5,
                    rgb(0xe6edf6).into(),
                    false,
                );
            }
        },
    )
    .w_full()
    .h(px(desired_h))
    .into_any_element()
}

// ---------------------------------------------------------------------------
// 8. XY Bar/Line Chart Renderer
// ---------------------------------------------------------------------------

fn render_xy_canvas(xy: XyChart) -> AnyElement {
    let desired_h = 250.0f32;

    canvas(
        |_, _, _| (),
        move |bounds: Bounds<Pixels>, _, window: &mut Window, cx: &mut App| {
            let w = f32::from(bounds.size.width).max(240.0);
            let h = f32::from(bounds.size.height).max(160.0);
            let ox = f32::from(bounds.origin.x);
            let oy = f32::from(bounds.origin.y);

            let title_h = if xy.title.is_some() { 26.0 } else { 10.0 };
            if let Some(title) = &xy.title {
                paint_text_centered(
                    window,
                    cx,
                    point(px(ox + w * 0.5), px(oy + 14.0)),
                    title,
                    13.0,
                    rgb(0xe6edf6).into(),
                    true,
                );
            }

            let plot_left = ox + 52.0;
            let plot_right = ox + w - 20.0;
            let plot_top = oy + title_h + 10.0;
            let plot_bot = oy + h - 32.0;
            let plot_w = (plot_right - plot_left).max(40.0);
            let plot_h = (plot_bot - plot_top).max(40.0);

            let mut min_v = xy.y_min.unwrap_or(0.0);
            let mut max_v = xy.y_max.unwrap_or(f32::NEG_INFINITY);
            let mut n_pts = xy.x_labels.len();
            for series in xy.bars.iter().chain(xy.lines.iter()) {
                n_pts = n_pts.max(series.len());
                for &v in series {
                    min_v = min_v.min(v);
                    max_v = max_v.max(v);
                }
            }
            if !max_v.is_finite() || max_v <= min_v {
                max_v = min_v + 10.0;
            }
            let span = (max_v - min_v).max(0.0001);
            let n_pts = n_pts.max(1);

            let grid_col: Hsla = rgb(0x232c3d).into();
            let axis_col: Hsla = rgb(0x475569).into();
            let dim_text: Hsla = rgb(0x8e9bb0).into();

            // Horizontal gridlines & Y ticks
            let ticks = 4usize;
            for t in 0..=ticks {
                let frac = t as f32 / ticks as f32;
                let y = plot_bot - frac * plot_h;
                let val = min_v + frac * span;
                let mut pb = PathBuilder::stroke(px(1.0));
                pb.move_to(point(px(plot_left), px(y)));
                pb.line_to(point(px(plot_right), px(y)));
                if let Ok(path) = pb.build() {
                    window.paint_path(path, if t == 0 { axis_col } else { grid_col });
                }
                let lbl = if (val - val.round()).abs() < 0.05 {
                    format!("{:.0}", val)
                } else {
                    format!("{:.1}", val)
                };
                paint_text_left(
                    window,
                    cx,
                    point(px(ox + 8.0), px(y - 6.0)),
                    &lbl,
                    10.0,
                    dim_text,
                    false,
                );
            }

            // Y-axis vertical line
            let mut yb = PathBuilder::stroke(px(1.2));
            yb.move_to(point(px(plot_left), px(plot_top)));
            yb.line_to(point(px(plot_left), px(plot_bot)));
            if let Ok(path) = yb.build() {
                window.paint_path(path, axis_col);
            }

            let step_x = plot_w / n_pts as f32;
            let val_to_y = |v: f32| plot_bot - ((v - min_v) / span).clamp(0.0, 1.0) * plot_h;

            // Bar series
            let bar_series_count = xy.bars.len().max(1);
            let group_w = (step_x * 0.64).min(48.0 * bar_series_count as f32);
            let single_bar_w = (group_w / bar_series_count as f32 - 2.0).max(6.0);
            for (si, series) in xy.bars.iter().enumerate() {
                let col: Hsla = rgb(CHART_PALETTE[si % CHART_PALETTE.len()]).into();
                for (i, &v) in series.iter().enumerate() {
                    let cx_p = plot_left + step_x * (i as f32 + 0.5);
                    let bx = cx_p - group_w * 0.5 + si as f32 * (single_bar_w + 2.0);
                    let by = val_to_y(v);
                    let bh = (plot_bot - by).max(2.0);
                    window.paint_quad(quad(
                        Bounds::new(point(px(bx), px(by)), size(px(single_bar_w), px(bh))),
                        Corners {
                            top_left: px(3.0),
                            top_right: px(3.0),
                            bottom_left: px(0.0),
                            bottom_right: px(0.0),
                        },
                        col,
                        Edges::all(px(0.0)),
                        col,
                        BorderStyle::Solid,
                    ));
                }
            }

            // Line series
            for (li, series) in xy.lines.iter().enumerate() {
                let col_idx = (xy.bars.len() + li) % CHART_PALETTE.len();
                let col: Hsla = rgb(CHART_PALETTE[col_idx]).into();
                if series.len() >= 2 {
                    let mut pb = PathBuilder::stroke(px(2.2));
                    for (i, &v) in series.iter().enumerate() {
                        let p = point(px(plot_left + step_x * (i as f32 + 0.5)), px(val_to_y(v)));
                        if i == 0 {
                            pb.move_to(p);
                        } else {
                            pb.line_to(p);
                        }
                    }
                    if let Ok(path) = pb.build() {
                        window.paint_path(path, col);
                    }
                }
                for (i, &v) in series.iter().enumerate() {
                    paint_circle(
                        window,
                        plot_left + step_x * (i as f32 + 0.5),
                        val_to_y(v),
                        4.0,
                        col,
                        Some((rgb(0x10141c).into(), 1.5)),
                    );
                }
            }

            // X-axis labels
            for i in 0..n_pts {
                let lbl = xy
                    .x_labels
                    .get(i)
                    .cloned()
                    .unwrap_or_else(|| format!("{}", i + 1));
                paint_text_centered(
                    window,
                    cx,
                    point(
                        px(plot_left + step_x * (i as f32 + 0.5)),
                        px(plot_bot + 14.0),
                    ),
                    &lbl,
                    10.5,
                    dim_text,
                    false,
                );
            }
        },
    )
    .w_full()
    .h(px(desired_h))
    .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_mermaid_flowchart_with_shapes_edges_and_subgraph() {
        let src = "\
flowchart LR
    subgraph Pipeline
        A[Start] -->|valid| B{Decision}
        B -.->|retry| A
    end
    B ==> C((Done))";
        let Some(Diagram::Flow(g)) = parse_mermaid(src) else {
            panic!("expected flowchart");
        };
        assert_eq!(g.direction, FlowDirection::LeftRight);
        assert_eq!(g.nodes.len(), 3);
        assert_eq!(g.nodes[0].shape, NodeShape::Rect);
        assert_eq!(g.nodes[1].shape, NodeShape::Diamond);
        assert_eq!(g.nodes[2].shape, NodeShape::Circle);
        assert_eq!(g.edges.len(), 3);
        assert_eq!(g.edges[0].label.as_deref(), Some("valid"));
        assert_eq!(g.edges[1].style, EdgeStyle::Dashed);
        assert_eq!(g.edges[2].style, EdgeStyle::Thick);
        assert_eq!(g.subgraphs.len(), 1);
        assert_eq!(g.subgraphs[0].title, "Pipeline");

        // Layout must handle cycles cleanly without infinite loops.
        let (placed, w, h) = compute_flow_layout(&g);
        assert_eq!(placed.len(), 3);
        assert!(w > 100.0 && h > 50.0);
    }

    #[test]
    fn parses_sequence_state_pie_and_xychart() {
        let seq = parse_mermaid(
            "sequenceDiagram\nparticipant U as User\nU->>API: Request\nAPI-->>U: 200 OK",
        );
        assert!(
            matches!(seq, Some(Diagram::Sequence(s)) if s.participants.len() == 2 && s.events.len() == 2)
        );

        let st =
            parse_mermaid("stateDiagram-v2\n[*] --> Idle\nIdle --> Active : start\nActive --> [*]");
        assert!(matches!(st, Some(Diagram::Flow(g)) if g.nodes.len() == 4 && g.edges.len() == 3));

        let pie = parse_mermaid("pie title Share\n\"Rust\" : 70\n\"Python\" : 30");
        assert!(
            matches!(pie, Some(Diagram::Pie(p)) if p.title.as_deref() == Some("Share") && p.slices.len() == 2)
        );

        let xy = parse_mermaid(
            "xychart-beta\ntitle \"Revenue\"\nx-axis [Q1, Q2, Q3]\ny-axis \"USD\" 0 --> 100\nbar [30, 60, 90]\nline [25, 55, 85]",
        );
        assert!(
            matches!(xy, Some(Diagram::Xy(c)) if c.bars.len() == 1 && c.lines.len() == 1 && c.x_labels.len() == 3)
        );
    }

    #[test]
    fn handles_er_diagrams_large_graphs_and_streaming_fallbacks() {
        // Unsupported or partial mid-stream Mermaid returns None (falls back to code block).
        assert!(parse_mermaid("").is_none());
        assert!(parse_mermaid("flowchart TD").is_none());
        assert!(parse_mermaid("gitGraph\n  commit").is_none());
        assert!(parse_mermaid("pie\n  title Empty").is_none());

        // ER diagram parsing
        let er = parse_mermaid(
            "erDiagram\nCUSTOMER ||--o{ ORDER : places\nORDER ||--|{ ITEM : contains",
        );
        assert!(matches!(er, Some(Diagram::Flow(g)) if g.nodes.len() == 3 && g.edges.len() == 2));

        // Large graph (120 nodes + back-edges) lays out cleanly without panicking or hanging.
        let mut big = String::from("flowchart TD\n");
        for i in 0..119 {
            big.push_str(&format!("N{i} --> N{}\n", i + 1));
        }
        big.push_str("N119 --> N0\nN50 --> N50\n");
        let Some(Diagram::Flow(g)) = parse_mermaid(&big) else {
            panic!("expected large flowchart");
        };
        assert_eq!(g.nodes.len(), 120);
        let (placed, w, h) = compute_flow_layout(&g);
        assert_eq!(placed.len(), 120);
        assert!(w > 100.0 && h > 100.0);
    }
}
