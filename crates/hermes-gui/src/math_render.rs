//! Native LaTeX math parser and GPUI typesetter for display formulas (`$$...$$`, `\[...\]`,
//! ```` ```math ````) and inline math prettification (`$...$`, `\(...\)`).

use gpui::{div, prelude::*, px, rgb, AnyElement, FontWeight, SharedString};

/// Hard recursion ceiling for LaTeX parsing, flattening, and GPUI rendering so deeply nested
/// inputs (e.g. `"{".repeat(100_000)` or nested `\frac`) can never trigger a stack overflow.
const MAX_LATEX_DEPTH: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AccentKind {
    Overline,
    Underline,
    Hat,
    Vec,
    Bar,
    Tilde,
    Dot,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MatrixKind {
    Plain,
    Paren,
    Bracket,
    Pipe,
    Brace,
    Cases,
    Aligned,
}

#[derive(Clone, Debug, PartialEq)]
pub enum MathNode {
    Text(String),
    Ident(String),
    Op(String),
    Func(String),
    Frac {
        num: Vec<MathNode>,
        den: Vec<MathNode>,
        bar: bool,
    },
    Sqrt {
        index: Option<Vec<MathNode>>,
        body: Vec<MathNode>,
    },
    BigOp {
        symbol: String,
        upright_word: bool,
        lower: Option<Vec<MathNode>>,
        upper: Option<Vec<MathNode>>,
    },
    Script {
        base: Box<MathNode>,
        sub: Option<Vec<MathNode>>,
        sup: Option<Vec<MathNode>>,
    },
    Accent {
        kind: AccentKind,
        body: Vec<MathNode>,
    },
    Delim {
        left: String,
        right: String,
        body: Vec<MathNode>,
    },
    Matrix {
        kind: MatrixKind,
        rows: Vec<Vec<Vec<MathNode>>>,
    },
    Styled {
        bold: bool,
        upright: bool,
        body: Vec<MathNode>,
    },
    Group(Vec<MathNode>),
    Space(f32),
    Lines(Vec<Vec<MathNode>>),
}

pub fn parse_latex(input: &str) -> Vec<MathNode> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    let mut parser = LatexParser::new(trimmed);
    let lines = parser.parse_lines_until(None);
    if lines.len() <= 1 {
        lines.into_iter().next().unwrap_or_default()
    } else {
        vec![MathNode::Lines(lines)]
    }
}

/// Render a display LaTeX formula into a structured GPUI element tree (fractions, roots, stacked
/// limits on big operators, superscripts/subscripts, matrices, cases, and Greek/math symbols).
pub fn render_math_formula(raw: &str) -> AnyElement {
    let nodes = parse_latex(raw);
    if nodes.is_empty() {
        return div().into_any_element();
    }
    let ctx = RenderCtx {
        size: 16.0,
        bold: false,
        force_upright: false,
        depth: 0,
    };
    div()
        .w_full()
        .flex()
        .flex_row()
        .flex_wrap()
        .items_center()
        .justify_center()
        .py_1()
        .child(render_nodes(&nodes, ctx))
        .into_any_element()
}

/// Convert inline LaTeX (`$...$` or `\(...\)`) into a clean Unicode display string for continuous
/// selectable text spans.
pub fn prettify_latex_inline(raw: &str) -> String {
    let nodes = parse_latex(raw);
    let mut out = String::new();
    flatten_nodes(&nodes, &mut out, 0);
    let trimmed = out.trim();
    if trimmed.is_empty() {
        raw.trim().to_owned()
    } else {
        trimmed.to_owned()
    }
}

#[derive(Clone, Copy)]
struct RenderCtx {
    size: f32,
    bold: bool,
    force_upright: bool,
    depth: usize,
}

impl RenderCtx {
    fn deeper(self) -> Self {
        Self {
            depth: self.depth.saturating_add(1),
            ..self
        }
    }

    fn script(self) -> Self {
        Self {
            size: (self.size * 0.72).max(10.0),
            depth: self.depth.saturating_add(1),
            ..self
        }
    }

    fn frac_part(self) -> Self {
        Self {
            size: (self.size * 0.86).max(11.0),
            depth: self.depth.saturating_add(1),
            ..self
        }
    }
}

fn math_color() -> gpui::Rgba {
    rgb(0xfef3c7)
}

fn math_op_color() -> gpui::Rgba {
    rgb(0x93c5fd)
}

fn math_dim_color() -> gpui::Rgba {
    rgb(0xfde68a)
}

fn render_nodes(nodes: &[MathNode], ctx: RenderCtx) -> AnyElement {
    if ctx.depth >= MAX_LATEX_DEPTH {
        let mut flat = String::new();
        flatten_nodes(nodes, &mut flat, 0);
        return div()
            .text_size(px(ctx.size))
            .text_color(math_color())
            .child(SharedString::from(flat))
            .into_any_element();
    }
    let next_ctx = ctx.deeper();
    let children: Vec<AnyElement> = nodes.iter().map(|n| render_node(n, next_ctx)).collect();
    div()
        .flex()
        .flex_row()
        .items_center()
        .children(children)
        .into_any_element()
}

fn render_node(node: &MathNode, ctx: RenderCtx) -> AnyElement {
    if ctx.depth >= MAX_LATEX_DEPTH {
        let mut flat = String::new();
        flatten_nodes(std::slice::from_ref(node), &mut flat, 0);
        return div()
            .text_size(px(ctx.size))
            .text_color(math_color())
            .child(SharedString::from(flat))
            .into_any_element();
    }
    let weight = if ctx.bold {
        FontWeight::BOLD
    } else {
        FontWeight::NORMAL
    };
    match node {
        MathNode::Text(s) => div()
            .text_size(px(ctx.size))
            .font_weight(weight)
            .text_color(math_color())
            .child(SharedString::from(s.clone()))
            .into_any_element(),
        MathNode::Ident(s) => {
            let mut el = div()
                .text_size(px(ctx.size))
                .font_weight(weight)
                .text_color(math_color())
                .child(SharedString::from(s.clone()));
            if !ctx.force_upright {
                el = el.italic();
            }
            el.into_any_element()
        }
        MathNode::Op(op) => {
            let pad = if matches!(
                op.as_str(),
                "=" | "+"
                    | "−"
                    | "±"
                    | "∓"
                    | "×"
                    | "÷"
                    | "≤"
                    | "≥"
                    | "≠"
                    | "≈"
                    | "≡"
                    | "→"
                    | "←"
                    | "↔"
                    | "⇒"
                    | "⇐"
                    | "⇔"
                    | "↦"
                    | "∈"
                    | "∉"
                    | "⊂"
                    | "⊆"
                    | "∪"
                    | "∩"
                    | "<"
                    | ">"
                    | "∼"
                    | "≃"
                    | "≅"
                    | "∝"
            ) {
                px(4.0)
            } else {
                px(1.5)
            };
            div()
                .px(pad)
                .text_size(px(ctx.size))
                .font_weight(weight)
                .text_color(math_op_color())
                .child(SharedString::from(op.clone()))
                .into_any_element()
        }
        MathNode::Func(name) => div()
            .px(px(2.0))
            .text_size(px(ctx.size))
            .font_weight(weight)
            .text_color(rgb(0xbfdbfe))
            .child(SharedString::from(name.clone()))
            .into_any_element(),
        MathNode::Space(w) => div().w(px(*w)).h(px(1.0)).into_any_element(),
        MathNode::Group(inner) => render_nodes(inner, ctx),
        MathNode::Styled {
            bold,
            upright,
            body,
        } => render_nodes(
            body,
            RenderCtx {
                bold: ctx.bold || *bold,
                force_upright: ctx.force_upright || *upright,
                ..ctx
            },
        ),
        MathNode::Frac { num, den, bar } => {
            let sub_ctx = ctx.frac_part();
            let mut col = div()
                .mx(px(3.0))
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .px(px(3.0))
                        .pb(px(1.0))
                        .child(render_nodes(num, sub_ctx)),
                );
            if *bar {
                col = col.child(div().w_full().h(px(1.2)).bg(math_dim_color()));
            }
            col = col.child(
                div()
                    .px(px(3.0))
                    .pt(px(1.0))
                    .child(render_nodes(den, sub_ctx)),
            );
            if *bar {
                col.into_any_element()
            } else {
                render_delim_box("(", ")", col.into_any_element(), ctx)
            }
        }
        MathNode::Sqrt { index, body } => {
            let mut row = div().mx(px(2.0)).flex().flex_row().items_end();
            if let Some(idx_nodes) = index {
                row = row.child(
                    div()
                        .pb(px(ctx.size * 0.45))
                        .mr(px(-2.0))
                        .child(render_nodes(idx_nodes, ctx.script())),
                );
            }
            row.child(
                div()
                    .text_size(px(ctx.size * 1.25))
                    .line_height(px(ctx.size * 1.15))
                    .text_color(math_dim_color())
                    .child("√"),
            )
            .child(
                div()
                    .border_t_1()
                    .border_color(math_dim_color())
                    .pt(px(1.0))
                    .px(px(3.0))
                    .child(render_nodes(body, ctx)),
            )
            .into_any_element()
        }
        MathNode::BigOp {
            symbol,
            upright_word,
            lower,
            upper,
        } => {
            let lim_ctx = ctx.script();
            let op_size = if *upright_word {
                ctx.size
            } else {
                (ctx.size * 1.65).min(28.0)
            };
            let mut col = div()
                .mx(px(4.0))
                .flex()
                .flex_col()
                .items_center()
                .justify_center();
            if let Some(up) = upper {
                col = col.child(div().mb(px(-2.0)).child(render_nodes(up, lim_ctx)));
            }
            col = col.child(
                div()
                    .text_size(px(op_size))
                    .line_height(px(op_size * 1.05))
                    .text_color(math_dim_color())
                    .child(SharedString::from(symbol.clone())),
            );
            if let Some(lo) = lower {
                col = col.child(div().mt(px(-2.0)).child(render_nodes(lo, lim_ctx)));
            }
            col.into_any_element()
        }
        MathNode::Script { base, sub, sup } => {
            let sc_ctx = ctx.script();
            let mut row = div().flex().flex_row().items_center();
            row = row.child(render_node(base, ctx));
            match (sup, sub) {
                (Some(sup_nodes), Some(sub_nodes)) => {
                    row = row.child(
                        div()
                            .ml(px(1.0))
                            .flex()
                            .flex_col()
                            .justify_between()
                            .gap(px(1.0))
                            .child(div().mb(px(1.0)).child(render_nodes(sup_nodes, sc_ctx)))
                            .child(div().mt(px(1.0)).child(render_nodes(sub_nodes, sc_ctx))),
                    );
                }
                (Some(sup_nodes), None) => {
                    row = row.child(
                        div()
                            .ml(px(1.0))
                            .pb(px(ctx.size * 0.48))
                            .child(render_nodes(sup_nodes, sc_ctx)),
                    );
                }
                (None, Some(sub_nodes)) => {
                    row = row.child(
                        div()
                            .ml(px(1.0))
                            .pt(px(ctx.size * 0.42))
                            .child(render_nodes(sub_nodes, sc_ctx)),
                    );
                }
                (None, None) => {}
            }
            row.into_any_element()
        }
        MathNode::Accent { kind, body } => match kind {
            AccentKind::Overline | AccentKind::Bar => div()
                .border_t_1()
                .border_color(math_dim_color())
                .pt(px(1.0))
                .px(px(1.0))
                .child(render_nodes(body, ctx))
                .into_any_element(),
            AccentKind::Underline => div()
                .border_b_1()
                .border_color(math_dim_color())
                .pb(px(1.0))
                .px(px(1.0))
                .child(render_nodes(body, ctx))
                .into_any_element(),
            AccentKind::Hat | AccentKind::Vec | AccentKind::Tilde | AccentKind::Dot => {
                let mark = match kind {
                    AccentKind::Hat => "ˆ",
                    AccentKind::Vec => "→",
                    AccentKind::Tilde => "˜",
                    AccentKind::Dot => "˙",
                    _ => "",
                };
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .child(
                        div()
                            .text_size(px((ctx.size * 0.65).max(9.0)))
                            .line_height(px((ctx.size * 0.55).max(8.0)))
                            .text_color(math_dim_color())
                            .child(mark),
                    )
                    .child(render_nodes(body, ctx))
                    .into_any_element()
            }
        },
        MathNode::Delim { left, right, body } => {
            render_delim_box(left, right, render_nodes(body, ctx), ctx)
        }
        MathNode::Matrix { kind, rows } => {
            let cell_ctx = ctx.frac_part();
            let row_els: Vec<AnyElement> = rows
                .iter()
                .map(|cols| {
                    let align_left = matches!(kind, MatrixKind::Cases | MatrixKind::Aligned);
                    let col_els: Vec<AnyElement> = cols
                        .iter()
                        .enumerate()
                        .map(|(col_idx, cell)| {
                            let mut d = div()
                                .px(px(6.0))
                                .py(px(2.0))
                                .flex()
                                .flex_row()
                                .items_center();
                            if align_left {
                                if matches!(kind, MatrixKind::Aligned) && col_idx % 2 == 0 {
                                    d = d.justify_end();
                                } else {
                                    d = d.justify_start();
                                }
                            } else {
                                d = d.justify_center();
                            }
                            d.child(render_nodes(cell, cell_ctx)).into_any_element()
                        })
                        .collect();
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_1()
                        .children(col_els)
                        .into_any_element()
                })
                .collect();
            let grid = div()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .children(row_els)
                .into_any_element();
            match kind {
                MatrixKind::Plain | MatrixKind::Aligned => grid,
                MatrixKind::Paren => render_delim_box("(", ")", grid, ctx),
                MatrixKind::Bracket => render_delim_box("[", "]", grid, ctx),
                MatrixKind::Pipe => render_delim_box("|", "|", grid, ctx),
                MatrixKind::Brace => render_delim_box("{", "}", grid, ctx),
                MatrixKind::Cases => render_delim_box("{", "", grid, ctx),
            }
        }
        MathNode::Lines(lines) => {
            let rows: Vec<AnyElement> = lines
                .iter()
                .map(|line| {
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .justify_center()
                        .child(render_nodes(line, ctx))
                        .into_any_element()
                })
                .collect();
            div()
                .flex()
                .flex_col()
                .items_center()
                .gap_2()
                .children(rows)
                .into_any_element()
        }
    }
}

fn render_delim_box(left: &str, right: &str, inner: AnyElement, ctx: RenderCtx) -> AnyElement {
    let c = math_dim_color();
    let mut row = div().mx(px(2.0)).flex().flex_row();

    match left {
        "" | "." => {}
        "[" => {
            row = row.child(
                div()
                    .w(px(6.0))
                    .my(px(1.0))
                    .border_l_2()
                    .border_t_1()
                    .border_b_1()
                    .border_color(c),
            );
        }
        "(" => {
            row = row.child(
                div()
                    .w(px(7.0))
                    .my(px(1.0))
                    .rounded_l_full()
                    .border_l_2()
                    .border_color(c),
            );
        }
        "|" => {
            row = row.child(div().w(px(3.0)).my(px(1.0)).border_l_2().border_color(c));
        }
        "‖" | "||" => {
            row = row.child(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(2.0))
                    .my(px(1.0))
                    .child(div().w(px(1.5)).h_full().bg(c))
                    .child(div().w(px(1.5)).h_full().bg(c)),
            );
        }
        "{" => {
            row = row.child(
                div()
                    .flex()
                    .items_center()
                    .pr(px(3.0))
                    .text_size(px(ctx.size * 1.5))
                    .text_color(c)
                    .child("{"),
            );
        }
        other => {
            row = row.child(
                div()
                    .flex()
                    .items_center()
                    .pr(px(2.0))
                    .text_size(px(ctx.size))
                    .text_color(c)
                    .child(SharedString::from(other.to_owned())),
            );
        }
    }

    row = row.child(div().px(px(2.0)).flex().items_center().child(inner));

    match right {
        "" | "." => {}
        "]" => {
            row = row.child(
                div()
                    .w(px(6.0))
                    .my(px(1.0))
                    .border_r_2()
                    .border_t_1()
                    .border_b_1()
                    .border_color(c),
            );
        }
        ")" => {
            row = row.child(
                div()
                    .w(px(7.0))
                    .my(px(1.0))
                    .rounded_r_full()
                    .border_r_2()
                    .border_color(c),
            );
        }
        "|" => {
            row = row.child(div().w(px(3.0)).my(px(1.0)).border_r_2().border_color(c));
        }
        "‖" | "||" => {
            row = row.child(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(2.0))
                    .my(px(1.0))
                    .child(div().w(px(1.5)).h_full().bg(c))
                    .child(div().w(px(1.5)).h_full().bg(c)),
            );
        }
        "}" => {
            row = row.child(
                div()
                    .flex()
                    .items_center()
                    .pl(px(3.0))
                    .text_size(px(ctx.size * 1.5))
                    .text_color(c)
                    .child("}"),
            );
        }
        other => {
            row = row.child(
                div()
                    .flex()
                    .items_center()
                    .pl(px(2.0))
                    .text_size(px(ctx.size))
                    .text_color(c)
                    .child(SharedString::from(other.to_owned())),
            );
        }
    }

    row.into_any_element()
}

// ---------------------------------------------------------------------------
// Recursive-descent LaTeX parser
// ---------------------------------------------------------------------------

struct LatexParser<'a> {
    src: &'a str,
    pos: usize,
    depth: usize,
}

impl<'a> LatexParser<'a> {
    fn new(src: &'a str) -> Self {
        Self {
            src,
            pos: 0,
            depth: 0,
        }
    }

    fn consume_raw_fallback(&mut self, stop_char: Option<char>) -> MathNode {
        let start = self.pos;
        let mut consumed = 0usize;
        while let Some(ch) = self.peek() {
            if stop_char == Some(ch) && consumed > 0 {
                break;
            }
            self.pos += ch.len_utf8();
            consumed += 1;
            if consumed >= 256 {
                break;
            }
        }
        MathNode::Text(self.src[start..self.pos].to_owned())
    }

    fn rest(&self) -> &'a str {
        &self.src[self.pos..]
    }

    fn peek(&self) -> Option<char> {
        self.rest().chars().next()
    }

    fn bump(&mut self) -> Option<char> {
        let ch = self.peek()?;
        self.pos += ch.len_utf8();
        Some(ch)
    }

    fn skip_ws(&mut self) {
        while let Some(ch) = self.peek() {
            if ch.is_whitespace() {
                self.pos += ch.len_utf8();
            } else {
                break;
            }
        }
    }

    fn starts_with(&self, s: &str) -> bool {
        self.rest().starts_with(s)
    }

    fn parse_lines_until(&mut self, stop_env: Option<&str>) -> Vec<Vec<MathNode>> {
        let mut lines = Vec::new();
        let mut cur = Vec::new();
        while self.pos < self.src.len() {
            self.skip_ws();
            if self.pos >= self.src.len() {
                break;
            }
            if let Some(env) = stop_env {
                if self.peek_end_env(env) {
                    break;
                }
            }
            if self.starts_with(r"\\") {
                self.pos += 2;
                // Optional spacing argument like `\\[4pt]`
                self.skip_optional_bracket_arg();
                lines.push(std::mem::take(&mut cur));
                continue;
            }
            if self.starts_with(r"\newline") {
                self.pos += r"\newline".len();
                lines.push(std::mem::take(&mut cur));
                continue;
            }
            if let Some(node) = self.parse_atom_with_scripts(false) {
                cur.push(node);
            } else {
                break;
            }
        }
        if !cur.is_empty() || lines.is_empty() {
            lines.push(cur);
        }
        lines
    }

    fn parse_nodes_until_close(
        &mut self,
        stop_char: Option<char>,
        stop_right: bool,
    ) -> Vec<MathNode> {
        let mut out = Vec::new();
        while self.pos < self.src.len() {
            self.skip_ws();
            let Some(ch) = self.peek() else {
                break;
            };
            if stop_char == Some(ch) {
                self.pos += ch.len_utf8();
                break;
            }
            if stop_right && self.starts_with(r"\right") {
                break;
            }
            if ch == '&' || self.starts_with(r"\\") || self.starts_with(r"\end{") {
                break;
            }
            if let Some(node) = self.parse_atom_with_scripts(stop_right) {
                out.push(node);
            } else {
                break;
            }
        }
        out
    }

    fn parse_atom_with_scripts(&mut self, inside_left: bool) -> Option<MathNode> {
        if self.depth >= MAX_LATEX_DEPTH {
            self.skip_ws();
            if self.pos >= self.src.len() {
                return None;
            }
            return Some(self.consume_raw_fallback(None));
        }
        let base = self.parse_atom(inside_left)?;
        let mut sub: Option<Vec<MathNode>> = None;
        let mut sup: Option<Vec<MathNode>> = None;

        loop {
            self.skip_ws();
            if self.starts_with(r"\limits") {
                self.pos += r"\limits".len();
                continue;
            }
            if self.starts_with(r"\nolimits") {
                self.pos += r"\nolimits".len();
                continue;
            }
            match self.peek() {
                Some('_') if sub.is_none() => {
                    self.pos += 1;
                    self.depth += 1;
                    sub = Some(self.parse_arg_nodes());
                    self.depth -= 1;
                }
                Some('^') if sup.is_none() => {
                    self.pos += 1;
                    self.depth += 1;
                    sup = Some(self.parse_arg_nodes());
                    self.depth -= 1;
                }
                Some('\'') => {
                    // Prime notation `f'`, `f''`
                    let mut primes = String::new();
                    while self.peek() == Some('\'') {
                        self.pos += 1;
                        primes.push('′');
                    }
                    let entry = sup.get_or_insert_with(Vec::new);
                    entry.push(MathNode::Text(primes));
                }
                _ => break,
            }
        }

        if sub.is_none() && sup.is_none() {
            return Some(base);
        }

        match base {
            MathNode::BigOp {
                symbol,
                upright_word,
                lower,
                upper,
            } => Some(MathNode::BigOp {
                symbol,
                upright_word,
                lower: lower.or(sub),
                upper: upper.or(sup),
            }),
            other => Some(MathNode::Script {
                base: Box::new(other),
                sub,
                sup,
            }),
        }
    }

    fn parse_arg_nodes(&mut self) -> Vec<MathNode> {
        self.skip_ws();
        if self.depth >= MAX_LATEX_DEPTH {
            let raw = self.parse_raw_group();
            return if raw.is_empty() {
                Vec::new()
            } else {
                vec![MathNode::Text(raw)]
            };
        }
        if self.peek() == Some('{') {
            self.pos += 1;
            self.depth += 1;
            let nodes = self.parse_nodes_until_close(Some('}'), false);
            self.depth -= 1;
            nodes
        } else if let Some(atom) = self.parse_atom(false) {
            vec![atom]
        } else {
            Vec::new()
        }
    }

    fn parse_raw_group(&mut self) -> String {
        self.skip_ws();
        if self.peek() != Some('{') {
            return self.bump().map(|c| c.to_string()).unwrap_or_default();
        }
        self.pos += 1;
        let start = self.pos;
        let mut depth = 1usize;
        while let Some(ch) = self.peek() {
            if ch == '\\' && self.pos + 1 < self.src.len() {
                self.pos += 1;
                let _ = self.bump();
                continue;
            }
            if ch == '{' {
                depth += 1;
            } else if ch == '}' {
                depth -= 1;
                if depth == 0 {
                    let content = &self.src[start..self.pos];
                    self.pos += 1;
                    return content.to_owned();
                }
            }
            self.pos += ch.len_utf8();
        }
        self.src[start..].to_owned()
    }

    fn parse_optional_bracket_nodes(&mut self) -> Option<Vec<MathNode>> {
        self.skip_ws();
        if self.peek() != Some('[') {
            return None;
        }
        self.pos += 1;
        Some(self.parse_nodes_until_close(Some(']'), false))
    }

    fn skip_optional_bracket_arg(&mut self) {
        let saved = self.pos;
        self.skip_ws();
        if self.peek() == Some('[') {
            self.pos += 1;
            while let Some(ch) = self.bump() {
                if ch == ']' {
                    return;
                }
            }
        }
        self.pos = saved;
    }

    fn parse_atom(&mut self, inside_left: bool) -> Option<MathNode> {
        self.skip_ws();
        let ch = self.peek()?;
        if ch == '}' || ch == '&' {
            return None;
        }
        if inside_left && self.starts_with(r"\right") {
            return None;
        }

        if ch == '{' {
            if self.depth >= MAX_LATEX_DEPTH {
                return Some(self.consume_raw_fallback(Some('}')));
            }
            self.pos += 1;
            self.depth += 1;
            let inner = self.parse_nodes_until_close(Some('}'), false);
            self.depth -= 1;
            return Some(MathNode::Group(inner));
        }

        if ch == '\\' {
            if self.depth >= MAX_LATEX_DEPTH {
                return Some(self.consume_raw_fallback(None));
            }
            self.depth += 1;
            let res = self.parse_command();
            self.depth -= 1;
            return res;
        }

        if ch == '~' {
            self.pos += 1;
            return Some(MathNode::Space(5.0));
        }

        // Number literal (digits + decimal point)
        if ch.is_ascii_digit() {
            let start = self.pos;
            while let Some(c) = self.peek() {
                if c.is_ascii_digit() || c == '.' {
                    self.pos += c.len_utf8();
                } else {
                    break;
                }
            }
            return Some(MathNode::Text(self.src[start..self.pos].to_owned()));
        }

        // Single variable letter
        if ch.is_ascii_alphabetic() {
            self.pos += 1;
            return Some(MathNode::Ident(ch.to_string()));
        }

        // Operators and punctuation
        self.pos += ch.len_utf8();
        match ch {
            '-' => Some(MathNode::Op("−".into())),
            '+' | '=' | '<' | '>' => Some(MathNode::Op(ch.to_string())),
            ',' | ';' | ':' | '!' | '?' | '(' | ')' | '[' | ']' | '|' | '/' => {
                Some(MathNode::Text(ch.to_string()))
            }
            '*' => Some(MathNode::Op("·".into())),
            other => Some(MathNode::Text(other.to_string())),
        }
    }

    fn parse_command(&mut self) -> Option<MathNode> {
        debug_assert_eq!(self.peek(), Some('\\'));
        self.pos += 1;
        let next = self.peek()?;

        // Escaped single-char commands: `\{`, `\}`, `\,`, `\;`, `\!`, `\ `, `\|`, `\%`, `\$`
        if !next.is_ascii_alphabetic() {
            self.pos += next.len_utf8();
            return match next {
                ',' => Some(MathNode::Space(3.0)),
                ':' | ';' => Some(MathNode::Space(5.0)),
                ' ' => Some(MathNode::Space(6.0)),
                '!' => Some(MathNode::Space(0.0)),
                '{' => Some(MathNode::Text("{".into())),
                '}' => Some(MathNode::Text("}".into())),
                '|' => Some(MathNode::Text("‖".into())),
                other => Some(MathNode::Text(other.to_string())),
            };
        }

        let start = self.pos;
        while let Some(c) = self.peek() {
            if c.is_ascii_alphabetic() {
                self.pos += 1;
            } else {
                break;
            }
        }
        let cmd = &self.src[start..self.pos];

        match cmd {
            "frac" | "dfrac" | "tfrac" | "cfrac" => {
                let num = self.parse_arg_nodes();
                let den = self.parse_arg_nodes();
                Some(MathNode::Frac {
                    num,
                    den,
                    bar: true,
                })
            }
            "binom" | "dbinom" | "tbinom" => {
                let num = self.parse_arg_nodes();
                let den = self.parse_arg_nodes();
                Some(MathNode::Frac {
                    num,
                    den,
                    bar: false,
                })
            }
            "sqrt" => {
                let index = self.parse_optional_bracket_nodes();
                let body = self.parse_arg_nodes();
                Some(MathNode::Sqrt { index, body })
            }
            "begin" => {
                let env = self.parse_raw_group();
                self.parse_environment(env.trim())
            }
            "left" => {
                let left = self.parse_delim_token();
                let body = self.parse_nodes_until_close(None, true);
                let right = if self.starts_with(r"\right") {
                    self.pos += r"\right".len();
                    self.parse_delim_token()
                } else {
                    String::new()
                };
                Some(MathNode::Delim { left, right, body })
            }
            "text" | "textrm" | "mbox" | "operatorname" => {
                let raw = self.parse_raw_group();
                Some(MathNode::Styled {
                    bold: false,
                    upright: true,
                    body: vec![MathNode::Text(raw)],
                })
            }
            "mathrm" | "rm" => {
                let body = self.parse_arg_nodes();
                Some(MathNode::Styled {
                    bold: false,
                    upright: true,
                    body,
                })
            }
            "mathbf" | "boldsymbol" | "bm" | "bf" => {
                let body = self.parse_arg_nodes();
                Some(MathNode::Styled {
                    bold: true,
                    upright: false,
                    body,
                })
            }
            "mathbb" => {
                let raw = self.parse_raw_group();
                let mapped: String = raw
                    .chars()
                    .map(|c| match c {
                        'R' => 'ℝ',
                        'N' => 'ℕ',
                        'Z' => 'ℤ',
                        'Q' => 'ℚ',
                        'C' => 'ℂ',
                        'P' => 'ℙ',
                        'E' => '𝔼',
                        'F' => '𝔽',
                        'H' => 'ℍ',
                        other => other,
                    })
                    .collect();
                Some(MathNode::Text(mapped))
            }
            "mathcal" | "mathscr" => {
                let raw = self.parse_raw_group();
                Some(MathNode::Ident(raw))
            }
            "overline" => Some(MathNode::Accent {
                kind: AccentKind::Overline,
                body: self.parse_arg_nodes(),
            }),
            "underline" => Some(MathNode::Accent {
                kind: AccentKind::Underline,
                body: self.parse_arg_nodes(),
            }),
            "hat" | "widehat" => Some(MathNode::Accent {
                kind: AccentKind::Hat,
                body: self.parse_arg_nodes(),
            }),
            "vec" | "overrightarrow" => Some(MathNode::Accent {
                kind: AccentKind::Vec,
                body: self.parse_arg_nodes(),
            }),
            "bar" => Some(MathNode::Accent {
                kind: AccentKind::Bar,
                body: self.parse_arg_nodes(),
            }),
            "tilde" | "widetilde" => Some(MathNode::Accent {
                kind: AccentKind::Tilde,
                body: self.parse_arg_nodes(),
            }),
            "dot" => Some(MathNode::Accent {
                kind: AccentKind::Dot,
                body: self.parse_arg_nodes(),
            }),
            "quad" => Some(MathNode::Space(12.0)),
            "qquad" => Some(MathNode::Space(20.0)),
            "displaystyle" | "textstyle" | "scriptstyle" | "big" | "Big" | "bigg" | "Bigg"
            | "bigl" | "bigr" | "Bigl" | "Bigr" | "limits" | "nolimits" => {
                Some(MathNode::Space(0.0))
            }
            // Big operators
            "sum" => Some(big_op("∑", false)),
            "prod" => Some(big_op("∏", false)),
            "coprod" => Some(big_op("∐", false)),
            "int" => Some(big_op("∫", false)),
            "iint" => Some(big_op("∬", false)),
            "iiint" => Some(big_op("∭", false)),
            "oint" => Some(big_op("∮", false)),
            "bigcup" => Some(big_op("⋃", false)),
            "bigcap" => Some(big_op("⋂", false)),
            "bigoplus" => Some(big_op("⨁", false)),
            "bigotimes" => Some(big_op("⨂", false)),
            "lim" => Some(big_op("lim", true)),
            "limsup" => Some(big_op("lim sup", true)),
            "liminf" => Some(big_op("lim inf", true)),
            "max" => Some(big_op("max", true)),
            "min" => Some(big_op("min", true)),
            "sup" => Some(big_op("sup", true)),
            "inf" => Some(big_op("inf", true)),
            "argmax" => Some(big_op("arg max", true)),
            "argmin" => Some(big_op("arg min", true)),
            // Standard math functions
            "sin" | "cos" | "tan" | "cot" | "sec" | "csc" | "arcsin" | "arccos" | "arctan"
            | "sinh" | "cosh" | "tanh" | "log" | "ln" | "exp" | "det" | "dim" | "ker" | "deg"
            | "gcd" | "lcm" | "arg" | "Pr" | "mod" | "bmod" => Some(MathNode::Func(cmd.to_owned())),
            "pmod" => {
                let inner = self.parse_arg_nodes();
                let mut body = vec![MathNode::Func("mod".into()), MathNode::Space(4.0)];
                body.extend(inner);
                Some(MathNode::Delim {
                    left: "(".into(),
                    right: ")".into(),
                    body,
                })
            }
            "not" => {
                self.skip_ws();
                if self.peek() == Some('=') {
                    self.pos += 1;
                    Some(MathNode::Op("≠".into()))
                } else if self.starts_with(r"\in") {
                    self.pos += 3;
                    Some(MathNode::Op("∉".into()))
                } else {
                    Some(MathNode::Op("¬".into()))
                }
            }
            other => {
                if let Some((sym, is_op)) = lookup_latex_symbol(other) {
                    if is_op {
                        Some(MathNode::Op(sym.to_owned()))
                    } else {
                        Some(MathNode::Text(sym.to_owned()))
                    }
                } else {
                    Some(MathNode::Text(format!("\\{other}")))
                }
            }
        }
    }

    fn parse_delim_token(&mut self) -> String {
        self.skip_ws();
        if self.peek() == Some('\\') {
            self.pos += 1;
            let Some(next) = self.peek() else {
                return String::new();
            };
            if !next.is_ascii_alphabetic() {
                self.pos += next.len_utf8();
                return match next {
                    '{' => "{".into(),
                    '}' => "}".into(),
                    '|' => "‖".into(),
                    other => other.to_string(),
                };
            }
            let start = self.pos;
            while let Some(c) = self.peek() {
                if c.is_ascii_alphabetic() {
                    self.pos += 1;
                } else {
                    break;
                }
            }
            return match &self.src[start..self.pos] {
                "langle" => "⟨".into(),
                "rangle" => "⟩".into(),
                "lfloor" => "⌊".into(),
                "rfloor" => "⌋".into(),
                "lceil" => "⌈".into(),
                "rceil" => "⌉".into(),
                "lbrace" => "{".into(),
                "rbrace" => "}".into(),
                "vert" => "|".into(),
                "Vert" => "‖".into(),
                other => other.to_owned(),
            };
        }
        match self.bump() {
            Some('.') => String::new(),
            Some(c) => c.to_string(),
            None => String::new(),
        }
    }

    fn peek_end_env(&self, env: &str) -> bool {
        let rest = self.rest().trim_start();
        rest.starts_with(&format!(r"\end{{{env}}}"))
    }

    fn consume_end_env(&mut self, env: &str) {
        self.skip_ws();
        let needle = format!(r"\end{{{env}}}");
        if self.starts_with(&needle) {
            self.pos += needle.len();
        }
    }

    fn parse_environment(&mut self, env: &str) -> Option<MathNode> {
        let kind = match env {
            "pmatrix" => MatrixKind::Paren,
            "bmatrix" => MatrixKind::Bracket,
            "vmatrix" | "Vmatrix" => MatrixKind::Pipe,
            "Bmatrix" => MatrixKind::Brace,
            "cases" | "dcases" => MatrixKind::Cases,
            "aligned" | "align" | "align*" | "eqnarray" | "eqnarray*" | "split" => {
                MatrixKind::Aligned
            }
            _ => MatrixKind::Plain,
        };

        // Optional column specification for `array`, e.g. `\begin{array}{cc}`
        if env == "array" {
            self.skip_ws();
            if self.peek() == Some('{') {
                let _ = self.parse_raw_group();
            }
        }

        let mut rows: Vec<Vec<Vec<MathNode>>> = Vec::new();
        let mut cur_row: Vec<Vec<MathNode>> = Vec::new();
        let mut cur_cell: Vec<MathNode> = Vec::new();

        while self.pos < self.src.len() {
            self.skip_ws();
            if self.peek_end_env(env) {
                self.consume_end_env(env);
                break;
            }
            if self.starts_with(r"\\") {
                self.pos += 2;
                self.skip_optional_bracket_arg();
                cur_row.push(std::mem::take(&mut cur_cell));
                rows.push(std::mem::take(&mut cur_row));
                continue;
            }
            if self.peek() == Some('&') {
                self.pos += 1;
                cur_row.push(std::mem::take(&mut cur_cell));
                continue;
            }
            if let Some(node) = self.parse_atom_with_scripts(false) {
                cur_cell.push(node);
            } else {
                // Avoid infinite loops on stray closing braces inside environments.
                if self.bump().is_none() {
                    break;
                }
            }
        }
        if !cur_cell.is_empty() || !cur_row.is_empty() {
            cur_row.push(cur_cell);
            rows.push(cur_row);
        }
        Some(MathNode::Matrix { kind, rows })
    }
}

fn big_op(symbol: &str, upright_word: bool) -> MathNode {
    MathNode::BigOp {
        symbol: symbol.to_owned(),
        upright_word,
        lower: None,
        upper: None,
    }
}

fn lookup_latex_symbol(cmd: &str) -> Option<(&'static str, bool)> {
    // Returns `(unicode, is_operator)`.
    Some(match cmd {
        // Greek lowercase
        "alpha" => ("α", false),
        "beta" => ("β", false),
        "gamma" => ("γ", false),
        "delta" => ("δ", false),
        "epsilon" | "varepsilon" => ("ε", false),
        "zeta" => ("ζ", false),
        "eta" => ("η", false),
        "theta" | "vartheta" => ("θ", false),
        "iota" => ("ι", false),
        "kappa" => ("κ", false),
        "lambda" => ("λ", false),
        "mu" => ("μ", false),
        "nu" => ("ν", false),
        "xi" => ("ξ", false),
        "pi" | "varpi" => ("π", false),
        "rho" | "varrho" => ("ρ", false),
        "sigma" | "varsigma" => ("σ", false),
        "tau" => ("τ", false),
        "upsilon" => ("υ", false),
        "phi" | "varphi" => ("φ", false),
        "chi" => ("χ", false),
        "psi" => ("ψ", false),
        "omega" => ("ω", false),
        // Greek uppercase
        "Gamma" => ("Γ", false),
        "Delta" => ("Δ", false),
        "Theta" => ("Θ", false),
        "Lambda" => ("Λ", false),
        "Xi" => ("Ξ", false),
        "Pi" => ("Π", false),
        "Sigma" => ("Σ", false),
        "Upsilon" => ("Υ", false),
        "Phi" => ("Φ", false),
        "Psi" => ("Ψ", false),
        "Omega" => ("Ω", false),
        // Misc math symbols
        "infty" => ("∞", false),
        "partial" => ("∂", false),
        "nabla" => ("∇", false),
        "forall" => ("∀", false),
        "exists" => ("∃", false),
        "nexists" => ("∄", false),
        "emptyset" | "varnothing" => ("∅", false),
        "hbar" => ("ℏ", false),
        "ell" => ("ℓ", false),
        "Re" => ("ℜ", false),
        "Im" => ("ℑ", false),
        "aleph" => ("ℵ", false),
        "angle" => ("∠", false),
        "degree" => ("°", false),
        "prime" => ("′", false),
        "ldots" | "dots" => ("…", false),
        "cdots" => ("⋯", false),
        "vdots" => ("⋮", false),
        "ddots" => ("⋱", false),
        // Operators & relations
        "pm" => ("±", true),
        "mp" => ("∓", true),
        "times" => ("×", true),
        "div" => ("÷", true),
        "cdot" => ("·", true),
        "ast" => ("∗", true),
        "star" => ("⋆", true),
        "circ" => ("∘", true),
        "bullet" => ("•", true),
        "oplus" => ("⊕", true),
        "otimes" => ("⊗", true),
        "odot" => ("⊙", true),
        "neq" | "ne" => ("≠", true),
        "leq" | "le" => ("≤", true),
        "geq" | "ge" => ("≥", true),
        "ll" => ("≪", true),
        "gg" => ("≫", true),
        "approx" => ("≈", true),
        "equiv" => ("≡", true),
        "sim" => ("∼", true),
        "simeq" => ("≃", true),
        "cong" => ("≅", true),
        "propto" => ("∝", true),
        "in" => ("∈", true),
        "notin" => ("∉", true),
        "ni" => ("∋", true),
        "subset" => ("⊂", true),
        "subseteq" => ("⊆", true),
        "supset" => ("⊃", true),
        "supseteq" => ("⊇", true),
        "cup" => ("∪", true),
        "cap" => ("∩", true),
        "setminus" => ("∖", true),
        "land" | "wedge" => ("∧", true),
        "lor" | "vee" => ("∨", true),
        "lnot" | "neg" => ("¬", true),
        "to" | "rightarrow" => ("→", true),
        "leftarrow" | "gets" => ("←", true),
        "leftrightarrow" => ("↔", true),
        "Rightarrow" | "implies" => ("⇒", true),
        "Leftarrow" => ("⇐", true),
        "Leftrightarrow" | "iff" => ("⇔", true),
        "mapsto" => ("↦", true),
        "uparrow" => ("↑", true),
        "downarrow" => ("↓", true),
        "perp" => ("⊥", true),
        "parallel" => ("∥", true),
        "therefore" => ("∴", true),
        "because" => ("∵", true),
        "mid" => ("∣", true),
        "langle" => ("⟨", false),
        "rangle" => ("⟩", false),
        "lfloor" => ("⌊", false),
        "rfloor" => ("⌋", false),
        "lceil" => ("⌈", false),
        "rceil" => ("⌉", false),
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// Inline Unicode flattener (for `$...$` / `\(...\)` inside paragraphs)
// ---------------------------------------------------------------------------

fn flatten_nodes(nodes: &[MathNode], out: &mut String, depth: usize) {
    if depth >= MAX_LATEX_DEPTH {
        if !out.ends_with('…') {
            out.push('…');
        }
        return;
    }
    let next = depth + 1;
    for node in nodes {
        match node {
            MathNode::Text(s) | MathNode::Ident(s) | MathNode::Func(s) => out.push_str(s),
            MathNode::Op(op) => {
                if !out.is_empty() && !out.ends_with(' ') && !out.ends_with('(') {
                    out.push(' ');
                }
                out.push_str(op);
                out.push(' ');
            }
            MathNode::Space(w) => {
                if *w > 0.0 && !out.ends_with(' ') {
                    out.push(' ');
                }
            }
            MathNode::Group(inner) | MathNode::Styled { body: inner, .. } => {
                flatten_nodes(inner, out, next);
            }
            MathNode::Frac { num, den, bar } => {
                let mut n = String::new();
                let mut d = String::new();
                flatten_nodes(num, &mut n, next);
                flatten_nodes(den, &mut d, next);
                let n = n.trim();
                let d = d.trim();
                if *bar {
                    if n.chars().count() > 1 {
                        out.push('(');
                        out.push_str(n);
                        out.push(')');
                    } else {
                        out.push_str(n);
                    }
                    out.push('/');
                    if d.chars().count() > 1 {
                        out.push('(');
                        out.push_str(d);
                        out.push(')');
                    } else {
                        out.push_str(d);
                    }
                } else {
                    out.push_str(&format!("C({n}, {d})"));
                }
            }
            MathNode::Sqrt { index, body } => {
                if let Some(idx) = index {
                    let mut idx_s = String::new();
                    flatten_nodes(idx, &mut idx_s, next);
                    if let Some(sup) = convert_script(&idx_s, true) {
                        out.push_str(&sup);
                    }
                }
                let mut b = String::new();
                flatten_nodes(body, &mut b, next);
                let b = b.trim();
                out.push('√');
                if b.chars().count() > 1 {
                    out.push('(');
                    out.push_str(b);
                    out.push(')');
                } else {
                    out.push_str(b);
                }
            }
            MathNode::BigOp {
                symbol,
                lower,
                upper,
                ..
            } => {
                out.push_str(symbol);
                if let Some(lo) = lower {
                    let mut s = String::new();
                    flatten_nodes(lo, &mut s, next);
                    let s = s.trim();
                    if let Some(sub) = convert_script(s, false) {
                        out.push_str(&sub);
                    } else {
                        out.push_str(&format!("_{{{s}}}"));
                    }
                }
                if let Some(up) = upper {
                    let mut s = String::new();
                    flatten_nodes(up, &mut s, next);
                    let s = s.trim();
                    if let Some(sup) = convert_script(s, true) {
                        out.push_str(&sup);
                    } else {
                        out.push_str(&format!("^{{{s}}}"));
                    }
                }
                out.push(' ');
            }
            MathNode::Script { base, sub, sup } => {
                flatten_nodes(std::slice::from_ref(base), out, next);
                if let Some(lo) = sub {
                    let mut s = String::new();
                    flatten_nodes(lo, &mut s, next);
                    let s = s.trim();
                    if let Some(pretty) = convert_script(s, false) {
                        out.push_str(&pretty);
                    } else {
                        out.push_str(&format!("_{s}"));
                    }
                }
                if let Some(up) = sup {
                    let mut s = String::new();
                    flatten_nodes(up, &mut s, next);
                    let s = s.trim();
                    if let Some(pretty) = convert_script(s, true) {
                        out.push_str(&pretty);
                    } else {
                        out.push_str(&format!("^{s}"));
                    }
                }
            }
            MathNode::Accent { kind, body } => {
                let mut b = String::new();
                flatten_nodes(body, &mut b, next);
                out.push_str(b.trim());
                match kind {
                    AccentKind::Hat => out.push('\u{0302}'),
                    AccentKind::Bar | AccentKind::Overline => out.push('\u{0304}'),
                    AccentKind::Vec => out.push('\u{20D7}'),
                    AccentKind::Tilde => out.push('\u{0303}'),
                    AccentKind::Dot => out.push('\u{0307}'),
                    AccentKind::Underline => {}
                }
            }
            MathNode::Delim { left, right, body } => {
                out.push_str(left);
                flatten_nodes(body, out, next);
                out.push_str(right);
            }
            MathNode::Matrix { rows, .. } => {
                out.push('[');
                for (r_idx, row) in rows.iter().enumerate() {
                    if r_idx > 0 {
                        out.push_str("; ");
                    }
                    for (c_idx, cell) in row.iter().enumerate() {
                        if c_idx > 0 {
                            out.push_str(", ");
                        }
                        flatten_nodes(cell, out, next);
                    }
                }
                out.push(']');
            }
            MathNode::Lines(lines) => {
                for (idx, line) in lines.iter().enumerate() {
                    if idx > 0 {
                        out.push('\n');
                    }
                    flatten_nodes(line, out, next);
                }
            }
        }
    }
}

fn convert_script(s: &str, sup: bool) -> Option<String> {
    let compact: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    compact
        .chars()
        .map(|c| {
            if sup {
                to_superscript(c)
            } else {
                to_subscript(c)
            }
        })
        .collect()
}

fn to_superscript(c: char) -> Option<char> {
    Some(match c {
        '0' => '⁰',
        '1' => '¹',
        '2' => '²',
        '3' => '³',
        '4' => '⁴',
        '5' => '⁵',
        '6' => '⁶',
        '7' => '⁷',
        '8' => '⁸',
        '9' => '⁹',
        '+' => '⁺',
        '-' | '−' => '⁻',
        '=' => '⁼',
        '(' => '⁽',
        ')' => '⁾',
        'n' => 'ⁿ',
        'i' => 'ⁱ',
        '′' => '′',
        _ => return None,
    })
}

fn to_subscript(c: char) -> Option<char> {
    Some(match c {
        '0' => '₀',
        '1' => '₁',
        '2' => '₂',
        '3' => '₃',
        '4' => '₄',
        '5' => '₅',
        '6' => '₆',
        '7' => '₇',
        '8' => '₈',
        '9' => '₉',
        '+' => '₊',
        '-' | '−' => '₋',
        '=' => '₌',
        '(' => '₍',
        ')' => '₎',
        'i' => 'ᵢ',
        'j' => 'ⱼ',
        'n' => 'ₙ',
        'k' => 'ₖ',
        'x' => 'ₓ',
        'a' => 'ₐ',
        'e' => 'ₑ',
        'o' => 'ₒ',
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fraction_sqrt_bigop_and_matrix() {
        let quad = parse_latex(r"x = \frac{-b \pm \sqrt{b^2 - 4ac}}{2a}");
        assert!(quad
            .iter()
            .any(|n| matches!(n, MathNode::Frac { bar: true, .. })));

        let sum = parse_latex(r"\sum_{i=1}^{n} x_i");
        assert!(matches!(
            &sum[0],
            MathNode::BigOp {
                symbol,
                lower: Some(_),
                upper: Some(_),
                ..
            } if symbol == "∑"
        ));

        let mat = parse_latex(r"\begin{pmatrix} a & b \\ c & d \end{pmatrix}");
        assert!(matches!(
            &mat[0],
            MathNode::Matrix {
                kind: MatrixKind::Paren,
                rows
            } if rows.len() == 2 && rows[0].len() == 2
        ));
    }

    #[test]
    fn prettifies_inline_math_cleanly() {
        assert_eq!(prettify_latex_inline("E = mc^2"), "E = mc²");
        assert_eq!(prettify_latex_inline(r"\sum_{i=1}^{n} x_i"), "∑ᵢ₌₁ⁿ xᵢ");
        assert_eq!(prettify_latex_inline(r"\frac{a+b}{2}"), "(a + b)/2");
        assert_eq!(prettify_latex_inline(r"\alpha \leq \beta"), "α ≤ β");
    }

    #[test]
    fn deeply_nested_latex_never_overflows_stack() {
        let braces = "{".repeat(100_000);
        let parsed = parse_latex(&braces);
        assert!(!parsed.is_empty());
        let inline = prettify_latex_inline(&braces);
        assert!(!inline.is_empty());

        let nested_frac = r"\frac{1}{".repeat(5_000);
        let frac_parsed = parse_latex(&nested_frac);
        assert!(!frac_parsed.is_empty());
        let frac_inline = prettify_latex_inline(&nested_frac);
        assert!(!frac_inline.is_empty());

        // Manually construct a deeply nested AST beyond MAX_LATEX_DEPTH and verify flattener bounds it.
        let mut deep = MathNode::Ident("x".into());
        for _ in 0..200 {
            deep = MathNode::Group(vec![deep]);
        }
        let mut flat = String::new();
        flatten_nodes(&[deep], &mut flat, 0);
        assert!(flat.contains('…'));
    }
}
