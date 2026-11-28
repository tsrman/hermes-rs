//! Native Mermaid diagram parser, layout engine, and GPUI vector canvas renderer.
//! Supports flowcharts (`flowchart`/`graph`), state diagrams (`stateDiagram`), ER diagrams
//! (`erDiagram`), sequence diagrams (`sequenceDiagram`), pie charts (`pie`), and XY bar/line
//! charts (`xychart-beta`/`xychart`).

mod parser;
mod render;

use gpui::AnyElement;

use parser::{
    parse_er_diagram, parse_flowchart, parse_pie_chart, parse_sequence_diagram,
    parse_state_diagram, parse_xy_chart,
};
use render::{render_flow_canvas, render_pie_canvas, render_sequence_canvas, render_xy_canvas};

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
        let (placed, w, h) = render::compute_flow_layout(&g);
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
        let (placed, w, h) = render::compute_flow_layout(&g);
        assert_eq!(placed.len(), 120);
        assert!(w > 100.0 && h > 100.0);
    }
}
