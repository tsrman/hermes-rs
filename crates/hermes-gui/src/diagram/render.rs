use std::f32::consts::PI;

use gpui::{
    canvas, font, point, prelude::*, px, quad, rgb, size, AnyElement, App, BorderStyle, Bounds,
    Corners, Edges, FontWeight, Hsla, PathBuilder, Pixels, Point, SharedString, TextRun, Window,
};

use super::{
    EdgeStyle, FlowDirection, FlowEdge, FlowGraph, NodeShape, PieChart, SeqEvent, SequenceDiagram,
    XyChart,
};

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
pub(super) struct LaidOutNode {
    cx: f32,
    cy: f32,
    w: f32,
    h: f32,
}

pub(super) fn compute_flow_layout(graph: &FlowGraph) -> (Vec<LaidOutNode>, f32, f32) {
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

pub(super) fn render_flow_canvas(graph: FlowGraph) -> AnyElement {
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

pub(super) fn render_sequence_canvas(seq: SequenceDiagram) -> AnyElement {
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

pub(super) fn render_pie_canvas(pie: PieChart) -> AnyElement {
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

pub(super) fn render_xy_canvas(xy: XyChart) -> AnyElement {
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
