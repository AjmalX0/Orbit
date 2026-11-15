//! A small layered layout that renders a workflow graph to SVG without
//! external tools (`orbit graph --format svg`, and the diagrams embedded in
//! `orbit docs --format html`).
//!
//! Layout: each step goes on the layer one below its deepest dependency
//! (longest path from the roots); within a layer, steps keep the IR's
//! execution order. A step's compensation sits to its right on the same
//! layer. Each outcome handler and event block is laid out the same way in
//! its own labelled panel below the main graph. Edges are drawn as curves
//! from the bottom of a step to the top of its dependent; edges that skip
//! layers may cross other nodes. The result is deterministic.

use super::graph::{scope_graph, Node, NodeKind};
use super::{scopes, Scope};
use orbit_ir::WorkflowIr;
use std::collections::HashMap;
use std::fmt::Write as _;

const CHAR_W: f64 = 7.4;
/// Gap between a step and its compensation, wide enough for the label.
const COMP_GAP: f64 = 96.0;
const LINE_H: f64 = 15.0;
const PAD_X: f64 = 10.0;
const PAD_Y: f64 = 8.0;
const GAP_X: f64 = 28.0;
const GAP_Y: f64 = 46.0;
const MARGIN: f64 = 16.0;
const PANEL_GAP: f64 = 28.0;
const TITLE_H: f64 = 26.0;

struct Placed {
    node: Node,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

fn size(n: &Node) -> (f64, f64) {
    let longest = n.lines.iter().map(|l| l.chars().count()).max().unwrap_or(1) as f64;
    (
        (longest * CHAR_W + 2.0 * PAD_X).max(80.0),
        n.lines.len() as f64 * LINE_H + 2.0 * PAD_Y,
    )
}

/// Horizontal space before a node in its row.
fn gap(n: &Node) -> f64 {
    if n.kind == NodeKind::Compensation {
        COMP_GAP
    } else {
        GAP_X
    }
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Lay out one scope with its top-left corner at (0, 0); returns the placed
/// nodes, the edges and the bounding size.
#[allow(clippy::type_complexity)]
fn layout(
    wf: &WorkflowIr,
    scope: Scope,
) -> (
    Vec<Placed>,
    Vec<(String, String, Option<String>, bool)>,
    f64,
    f64,
) {
    let g = scope_graph(wf, scope, false);
    // Layer of each step node (compensations share their step's layer).
    let mut layer: HashMap<String, usize> = HashMap::new();
    let mut rows: Vec<Vec<Node>> = vec![];
    for n in &g.nodes {
        let l = if n.kind == NodeKind::Compensation {
            let owner = n.id.trim_end_matches("__undo");
            layer.get(owner).copied().unwrap_or(0)
        } else {
            g.edges
                .iter()
                .filter(|e| e.1 == n.id && !e.3)
                .filter_map(|e| layer.get(&e.0))
                .map(|l| l + 1)
                .max()
                .unwrap_or(0)
        };
        layer.insert(n.id.clone(), l);
        if rows.len() <= l {
            rows.resize(l + 1, vec![]);
        }
        rows[l].push(n.clone());
    }
    let widths: Vec<f64> = rows
        .iter()
        .map(|r| {
            r.iter()
                .enumerate()
                .map(|(i, n)| size(n).0 + if i == 0 { 0.0 } else { gap(n) })
                .sum::<f64>()
        })
        .collect();
    let total_w = widths.iter().copied().fold(0.0, f64::max);
    let mut placed = vec![];
    let mut y = 0.0;
    for (row, w) in rows.iter().zip(&widths) {
        let h = row.iter().map(|n| size(n).1).fold(0.0, f64::max);
        let mut x = (total_w - w) / 2.0;
        for (i, n) in row.iter().enumerate() {
            if i > 0 {
                x += gap(n) - GAP_X;
            }
            let (nw, nh) = size(n);
            placed.push(Placed {
                node: n.clone(),
                x,
                y: y + (h - nh) / 2.0,
                w: nw,
                h: nh,
            });
            x += nw + GAP_X;
        }
        y += h + GAP_Y;
    }
    let total_h = (y - GAP_Y).max(0.0);
    (placed, g.edges, total_w, total_h)
}

const STYLE: &str = r#"<style>
.orbit-graph { --ink: #1c1917; --muted: #57534e; --line: #a8a29e; --bg: #faf9f7; --band: #f3f1ed; --accent: #c2410c; }
@media (prefers-color-scheme: dark) { .orbit-graph { --ink: #ece9e4; --muted: #a8a29e; --line: #6b6560; --bg: #0f0d0c; --band: #161412; --accent: #fb923c; } }
.orbit-graph text { font: 12px ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; fill: var(--muted); }
.orbit-graph text.name { font-weight: 600; fill: var(--ink); }
.orbit-graph .node { fill: var(--bg); stroke: var(--ink); stroke-width: 1.2; }
.orbit-graph .conditional { stroke: var(--accent); }
.orbit-graph .compensation { stroke-dasharray: 5 3; stroke: var(--muted); }
.orbit-graph .edge { fill: none; stroke: var(--line); stroke-width: 1.3; }
.orbit-graph .edge.dashed { stroke-dasharray: 5 3; }
.orbit-graph .panel { fill: var(--band); stroke: var(--line); stroke-dasharray: 4 3; }
.orbit-graph .title { font-weight: 600; fill: var(--ink); }
.orbit-graph .arrow { fill: var(--line); }
</style>"#;

/// The workflow as a standalone SVG document.
pub fn svg(wf: &WorkflowIr) -> String {
    let mut panels = vec![];
    for scope in scopes(wf) {
        panels.push((scope, layout(wf, scope)));
    }
    let width = panels
        .iter()
        .map(|(s, (_, _, w, _))| {
            if *s == Scope::Main {
                *w
            } else {
                w + 2.0 * MARGIN
            }
        })
        .fold(0.0, f64::max)
        + 2.0 * MARGIN;
    let mut body = String::new();
    let mut y = MARGIN;
    for (scope, (placed, edges, w, h)) in &panels {
        let (ox, oy) = if *scope == Scope::Main {
            ((width - w) / 2.0, y)
        } else {
            let px = (width - w - 2.0 * MARGIN) / 2.0;
            let _ = writeln!(
                body,
                "<rect class=\"panel\" x=\"{px:.1}\" y=\"{y:.1}\" width=\"{:.1}\" height=\"{:.1}\" rx=\"6\"/>",
                w + 2.0 * MARGIN,
                h + TITLE_H + MARGIN
            );
            let _ = writeln!(
                body,
                "<text class=\"title\" x=\"{:.1}\" y=\"{:.1}\">{}</text>",
                px + 10.0,
                y + 17.0,
                esc(&scope.label())
            );
            (px + MARGIN, y + TITLE_H)
        };
        let at: HashMap<&str, &Placed> = placed.iter().map(|p| (p.node.id.as_str(), p)).collect();
        for (from, to, label, dashed) in edges {
            let (Some(a), Some(b)) = (at.get(from.as_str()), at.get(to.as_str())) else {
                continue;
            };
            let class = if *dashed { "edge dashed" } else { "edge" };
            if b.node.kind == NodeKind::Compensation {
                // Same layer: a horizontal link from the step to its compensation.
                let (x1, y1) = (ox + a.x + a.w, oy + a.y + a.h / 2.0);
                let (x2, y2) = (ox + b.x, oy + b.y + b.h / 2.0);
                let _ = writeln!(
                    body,
                    "<path class=\"{class}\" d=\"M{x1:.1},{y1:.1} L{:.1},{y2:.1}\" marker-end=\"url(#arrow)\"/>",
                    x2 - 2.0
                );
                if let Some(l) = label {
                    let _ = writeln!(
                        body,
                        "<text x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"middle\">{}</text>",
                        (x1 + x2) / 2.0,
                        y1 - 5.0,
                        esc(l)
                    );
                }
                continue;
            }
            let (x1, y1) = (ox + a.x + a.w / 2.0, oy + a.y + a.h);
            let (x2, y2) = (ox + b.x + b.w / 2.0, oy + b.y);
            let my = (y1 + y2) / 2.0;
            let _ = writeln!(
                body,
                "<path class=\"{class}\" d=\"M{x1:.1},{y1:.1} C{x1:.1},{my:.1} {x2:.1},{my:.1} {x2:.1},{:.1}\" marker-end=\"url(#arrow)\"/>",
                y2 - 2.0
            );
        }
        for p in placed {
            let class = match p.node.kind {
                NodeKind::Step | NodeKind::Signal => "node",
                NodeKind::Conditional => "node conditional",
                NodeKind::Compensation => "node compensation",
            };
            let _ = writeln!(
                body,
                "<rect class=\"{class}\" x=\"{:.1}\" y=\"{:.1}\" width=\"{:.1}\" height=\"{:.1}\" rx=\"{}\"/>",
                ox + p.x,
                oy + p.y,
                p.w,
                p.h,
                if p.node.kind == NodeKind::Compensation { 14 } else { 5 }
            );
            for (i, line) in p.node.lines.iter().enumerate() {
                let cls = if i == 0 { " class=\"name\"" } else { "" };
                let _ = writeln!(
                    body,
                    "<text{cls} x=\"{:.1}\" y=\"{:.1}\">{}</text>",
                    ox + p.x + PAD_X,
                    oy + p.y + PAD_Y + LINE_H * (i as f64 + 1.0) - 4.0,
                    esc(line)
                );
            }
        }
        y = oy + h + if *scope == Scope::Main { 0.0 } else { MARGIN } + PANEL_GAP;
    }
    let height = y - PANEL_GAP + MARGIN;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "<svg xmlns=\"http://www.w3.org/2000/svg\" class=\"orbit-graph\" width=\"{width:.0}\" height=\"{height:.0}\" viewBox=\"0 0 {width:.0} {height:.0}\" role=\"img\" aria-label=\"Step graph of workflow {}\">",
        esc(&wf.name)
    );
    let _ = writeln!(out, "<title>{}@{}</title>", esc(&wf.name), esc(&wf.version));
    out.push_str(STYLE);
    out.push('\n');
    out.push_str("<defs><marker id=\"arrow\" viewBox=\"0 0 10 10\" refX=\"9\" refY=\"5\" markerWidth=\"7\" markerHeight=\"7\" orient=\"auto-start-reverse\"><path class=\"arrow\" d=\"M0,0 L10,5 L0,10 z\"/></marker></defs>\n");
    out.push_str(&body);
    out.push_str("</svg>\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile_files;
    use std::path::PathBuf;

    #[test]
    fn renders_well_formed_deterministic_svg() {
        let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/checkout.orbit");
        let c = compile_files(&[p]);
        let s = svg(&c.workflows[0]);
        assert_eq!(s, svg(&c.workflows[0]));
        assert!(s.starts_with("<svg xmlns=\"http://www.w3.org/2000/svg\""));
        assert!(s.trim_end().ends_with("</svg>"));
        assert!(s.contains(">on failure</text>"));
        assert!(s.contains(">undo reserve</text>"));
        // Every opened element is closed (a cheap well-formedness check).
        assert_eq!(s.matches("<text").count(), s.matches("</text>").count());
        assert!(!s.contains("NaN"));
    }
}
