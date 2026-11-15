//! `orbit graph`: a workflow as a Mermaid flowchart, a Graphviz digraph or a
//! Mermaid sequence diagram.
//!
//! Output is deterministic: steps appear in the IR's execution order (a
//! stable topological order), dependencies are sorted, handlers follow the
//! order `failure`, `cancel`, `timeout`, and event blocks are sorted by signal.
//! The same definition always renders to the same text, so diagrams can be
//! committed and reviewed as diffs.

use super::pretty;
use super::{retry_summary, scope_steps, scopes, timeout_summary, Scope};
use orbit_ir::{Action, StepIr, Type, WorkflowIr, WORKFLOW_CONTRACT_PREFIX};
use std::fmt::Write as _;

/// One rendered step: its node id, its label lines and how it is drawn.
#[derive(Debug, Clone)]
pub struct Node {
    pub id: String,
    pub lines: Vec<String>,
    pub kind: NodeKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    Step,
    Conditional,
    Compensation,
    Signal,
    /// The workflow's `continue_as_new`: after the final steps, with a dashed
    /// `next run` edge back to the first steps.
    ContinueAsNew,
}

/// Node id of the `continue_as_new` node (a keyword, so never a step id).
pub const CONTINUE_ID: &str = "continue_as_new";

/// The label lines of a `continue_as_new`: `continue as new`, its condition
/// (`always` without `when`) and the fields of the next run's input.
pub fn continue_lines(c: &orbit_ir::ContinueAsNewIr) -> Vec<String> {
    vec![
        "continue as new".into(),
        match &c.condition {
            Some(e) => format!("when {}", pretty::expr(e)),
            None => "always".into(),
        },
        format!("next input {}", input_fields(&c.input)),
    ]
}

/// `{ a, b }` for an object literal (the fields it sets), the expression
/// otherwise.
pub fn input_fields(e: &orbit_ir::Expr) -> String {
    match e {
        orbit_ir::Expr::Object { fields } if fields.is_empty() => "{}".into(),
        orbit_ir::Expr::Object { fields } => format!(
            "{{ {} }}",
            fields
                .iter()
                .map(|f| f.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
        other => pretty::expr(other),
    }
}

/// Node id prefix of a scope: ``, `on_failure__`, `on_event_location__`.
pub fn prefix(scope: Scope) -> String {
    match scope {
        Scope::Main => String::new(),
        Scope::Handler(k) => format!("on_{k}__"),
        Scope::Event(s) => format!("on_event_{s}__"),
    }
}

/// The label lines of a step: name, action, then annotations.
pub fn step_lines(s: &StepIr) -> Vec<String> {
    let mut lines = vec![s.id.clone()];
    match &s.action {
        Action::Task(i) => {
            let mut l = format!("{}@{}", i.task, i.contract_version);
            if let Some(lang) = &i.language {
                l.push_str(&format!(" · {lang}"));
            }
            lines.push(l);
        }
        Action::Workflow(i) => lines.push(format!(
            "child workflow {}",
            i.task.trim_start_matches(WORKFLOW_CONTRACT_PREFIX)
        )),
        Action::Sleep { duration_ms } => {
            lines.push(format!("sleep {}", super::duration(*duration_ms)))
        }
        Action::Await { signal, .. } => lines.push(format!("await signal {signal}")),
        Action::Compute { .. } => lines.push("compute".into()),
    }
    if let Some(f) = &s.for_each {
        lines.push(match f.max_parallel {
            Some(n) => format!("for each {} (max {n} parallel)", f.variable),
            None => format!("for each {}", f.variable),
        });
    }
    if let Some(c) = &s.condition {
        lines.push(format!("when {}", pretty::expr(c)));
    }
    if let Some(i) = s.invocation() {
        if let Some(r) = retry_summary(&i.retry) {
            lines.push(format!("retry {r}"));
        }
        if i.idempotency_key.is_some() {
            lines.push("idempotency key".into());
        }
    }
    if let Some(t) = timeout_summary(s) {
        lines.push(t);
    }
    lines
}

/// Nodes and edges of one scope.
pub struct ScopeGraph {
    pub nodes: Vec<Node>,
    /// `(from, to, label, dashed)`.
    pub edges: Vec<(String, String, Option<String>, bool)>,
}

pub fn scope_graph(wf: &WorkflowIr, scope: Scope, signal_nodes: bool) -> ScopeGraph {
    let p = prefix(scope);
    let mut nodes = vec![];
    let mut edges = vec![];
    let mut signals_drawn: Vec<String> = vec![];
    for s in scope_steps(wf, scope) {
        let id = format!("{p}{}", s.id);
        if let (true, Action::Await { signal, .. }) = (signal_nodes, &s.action) {
            let sid = format!("{p}signal__{signal}");
            if !signals_drawn.contains(&sid) {
                signals_drawn.push(sid.clone());
                nodes.push(Node {
                    id: sid.clone(),
                    lines: vec![format!("signal {signal}")],
                    kind: NodeKind::Signal,
                });
            }
            edges.push((sid, id.clone(), None, true));
        }
        nodes.push(Node {
            id: id.clone(),
            lines: step_lines(s),
            kind: if s.condition.is_some() {
                NodeKind::Conditional
            } else {
                NodeKind::Step
            },
        });
        for d in &s.dependencies {
            edges.push((format!("{p}{d}"), id.clone(), None, false));
        }
        if let Some(c) = &s.compensation {
            let cid = format!("{id}__undo");
            let mut lines = vec![
                format!("undo {}", s.id),
                format!("{}@{}", c.task, c.contract_version),
            ];
            if let Some(r) = retry_summary(&c.retry) {
                lines.push(format!("retry {r}"));
            }
            nodes.push(Node {
                id: cid.clone(),
                lines,
                kind: NodeKind::Compensation,
            });
            edges.push((id.clone(), cid, Some("compensate".into()), true));
        }
    }
    if let (Scope::Main, Some(c)) = (scope, &wf.continue_as_new) {
        // Evaluated once every step has finished: after the steps nothing
        // depends on; the next run starts again at the steps without
        // dependencies.
        let steps = scope_steps(wf, scope);
        let sinks = steps
            .iter()
            .filter(|s| !steps.iter().any(|t| t.dependencies.contains(&s.id)));
        let roots = steps.iter().filter(|s| s.dependencies.is_empty());
        nodes.push(Node {
            id: CONTINUE_ID.into(),
            lines: continue_lines(c),
            kind: NodeKind::ContinueAsNew,
        });
        for s in sinks {
            edges.push((s.id.clone(), CONTINUE_ID.into(), None, false));
        }
        for s in roots {
            edges.push((
                CONTINUE_ID.into(),
                s.id.clone(),
                Some("next run".into()),
                true,
            ));
        }
    }
    ScopeGraph { nodes, edges }
}

impl ScopeGraph {
    /// The edges to print with node `n`: those into it, except the
    /// `next run` edges out of `continue_as_new`, which follow that node.
    pub fn edges_at(&self, n: &Node) -> Vec<&(String, String, Option<String>, bool)> {
        self.edges
            .iter()
            .filter(|e| {
                if n.kind == NodeKind::ContinueAsNew {
                    e.1 == n.id || e.0 == n.id
                } else {
                    e.1 == n.id && e.0 != CONTINUE_ID
                }
            })
            .collect()
    }
}

/// Header facts shown as a comment: version, hash, timeout, limits.
fn header(wf: &WorkflowIr) -> Vec<String> {
    let mut out = vec![format!("{}@{}", wf.name, wf.version)];
    if !wf.definition_hash.is_empty() {
        out.push(wf.definition_hash.clone());
    }
    if let Some(t) = wf.timeout_ms {
        out.push(format!("workflow timeout {}", super::duration(t)));
    }
    if let Some(l) = &wf.limits {
        out.push(format!("limits {}", limits_summary(l)));
    }
    out
}

pub fn limits_summary(l: &orbit_ir::WorkflowLimits) -> String {
    let mut parts = vec![];
    if let Some(n) = l.max_fan_out {
        parts.push(format!("max_fan_out {n}"));
    }
    if let Some(n) = l.max_history_events {
        parts.push(format!("max_history_events {n}"));
    }
    if let Some(n) = l.max_payload_bytes {
        parts.push(format!("max_payload_bytes {n}"));
    }
    parts.join(", ")
}

fn mermaid_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("#quot;"),
            '<' => out.push_str("#lt;"),
            '>' => out.push_str("#gt;"),
            '#' => out.push_str("#35;"),
            _ => out.push(c),
        }
    }
    out
}

fn mermaid_label(n: &Node) -> String {
    let mut parts = vec![];
    for (i, l) in n.lines.iter().enumerate() {
        let l = mermaid_escape(l);
        if i == 0 {
            parts.push(l);
        } else {
            parts.push(format!("<small>{l}</small>"));
        }
    }
    parts.join("<br/>")
}

fn mermaid_node(n: &Node) -> String {
    let label = mermaid_label(n);
    match n.kind {
        NodeKind::Step => format!("{}[\"{label}\"]", n.id),
        NodeKind::Conditional => format!("{}{{{{\"{label}\"}}}}", n.id),
        NodeKind::Compensation => format!("{}([\"{label}\"]):::compensation", n.id),
        NodeKind::Signal => format!("{}>\"{label}\"]:::signal", n.id),
        NodeKind::ContinueAsNew => format!("{}[[\"{label}\"]]", n.id),
    }
}

fn mermaid_edge(from: &str, to: &str, label: &Option<String>, dashed: bool) -> String {
    match (label, dashed) {
        (Some(l), true) => format!("{from} -. {l} .-> {to}"),
        (None, true) => format!("{from} -.-> {to}"),
        (Some(l), false) => format!("{from} -- {l} --> {to}"),
        (None, false) => format!("{from} --> {to}"),
    }
}

/// Mermaid flowchart. Steps are rectangles, conditional steps hexagons,
/// compensations dashed stadiums linked from the step they undo, signals
/// flags feeding their `await` step, and each outcome handler and event block
/// a subgraph. A `continue_as_new` is a subroutine node after the final
/// steps, with a dashed `next run` edge back to the first steps.
pub fn mermaid(wf: &WorkflowIr) -> String {
    let mut out = String::from("flowchart TD\n");
    for h in header(wf) {
        let _ = writeln!(out, "    %% {h}");
    }
    out.push_str("    classDef compensation stroke-dasharray: 4 3\n");
    out.push_str("    classDef signal stroke-dasharray: 2 2\n");
    for scope in scopes(wf) {
        let g = scope_graph(wf, scope, true);
        let indent = if scope == Scope::Main {
            "    "
        } else {
            let p = prefix(scope);
            let _ = writeln!(
                out,
                "    subgraph {}[\"{}\"]",
                p.trim_end_matches('_'),
                mermaid_escape(&scope.label())
            );
            out.push_str("        direction TB\n");
            "        "
        };
        for n in &g.nodes {
            let _ = writeln!(out, "{indent}{}", mermaid_node(n));
            for (from, to, label, dashed) in g.edges_at(n) {
                let _ = writeln!(out, "{indent}{}", mermaid_edge(from, to, label, *dashed));
            }
        }
        if scope != Scope::Main {
            out.push_str("    end\n");
        }
    }
    out
}

fn dot_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Graphviz DOT. Same elements as [`mermaid`]: handlers and event blocks are
/// dashed clusters.
pub fn dot(wf: &WorkflowIr) -> String {
    let mut out = format!("digraph \"{}\" {{\n", dot_escape(&wf.name));
    for h in header(wf) {
        let _ = writeln!(out, "  // {h}");
    }
    out.push_str("  rankdir=TB;\n");
    out.push_str("  node [shape=box, style=rounded, fontname=\"Helvetica\", fontsize=11];\n");
    out.push_str("  edge [fontname=\"Helvetica\", fontsize=10];\n");
    for scope in scopes(wf) {
        let g = scope_graph(wf, scope, true);
        let indent = if scope == Scope::Main {
            "  "
        } else {
            let _ = writeln!(
                out,
                "  subgraph \"cluster_{}\" {{\n    label=\"{}\";\n    style=dashed;",
                prefix(scope).trim_end_matches('_'),
                dot_escape(&scope.label())
            );
            "    "
        };
        for n in &g.nodes {
            let label = n
                .lines
                .iter()
                .map(|l| dot_escape(l))
                .collect::<Vec<_>>()
                .join("\\n");
            let attrs = match n.kind {
                NodeKind::Step => "",
                NodeKind::Conditional => ", shape=hexagon, style=\"\"",
                NodeKind::Compensation => ", style=\"rounded,dashed\"",
                NodeKind::Signal => ", shape=cds, style=dashed",
                NodeKind::ContinueAsNew => ", peripheries=2",
            };
            let _ = writeln!(out, "{indent}\"{}\" [label=\"{label}\"{attrs}];", n.id);
            for (from, to, label, dashed) in g.edges_at(n) {
                let mut attrs = vec![];
                if *dashed {
                    attrs.push("style=dashed".to_string());
                }
                if from == CONTINUE_ID {
                    // A back edge: keep the steps ranked top to bottom.
                    attrs.push("constraint=false".to_string());
                }
                if let Some(l) = label {
                    attrs.push(format!("label=\"{}\"", dot_escape(l)));
                }
                let attrs = if attrs.is_empty() {
                    String::new()
                } else {
                    format!(" [{}]", attrs.join(", "))
                };
                let _ = writeln!(out, "{indent}\"{from}\" -> \"{to}\"{attrs};");
            }
        }
        if scope != Scope::Main {
            out.push_str("  }\n");
        }
    }
    out.push_str("}\n");
    out
}

fn participant(task: &str) -> String {
    let service = task.rsplit_once('.').map(|(a, _)| a).unwrap_or(task);
    service
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

/// Short description of a type for a reply arrow: a record's name, or its
/// field names.
fn short_type(t: &Type) -> String {
    match t {
        Type::Record { name: Some(n), .. } => n.clone(),
        Type::Record { name: None, fields } if fields.is_empty() => "{}".into(),
        Type::Record { name: None, fields } => format!(
            "{{ {} }}",
            fields
                .iter()
                .map(|f| f.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Type::List { item } => format!("List<{}>", short_type(item)),
        Type::Optional { inner } => format!("{}?", short_type(inner)),
        other => other.to_string(),
    }
}

fn seq_text(s: &str) -> String {
    s.replace(';', ",").replace('#', "#35;")
}

/// Mermaid sequence diagram of the calls a workflow makes: worker tasks
/// (one participant per task namespace, `payments` for `payments.charge`),
/// child workflows, signals from outside (`Sender`) and timers. Steps appear
/// in execution order; steps that can run concurrently are drawn one after
/// the other. Conditional steps are `opt` blocks, `for` steps `loop` blocks,
/// a `continue_as_new` is a message from the workflow to itself (in an `opt`
/// block when it has a condition), and compensations and handlers are `opt`
/// blocks at the end.
pub fn sequence(wf: &WorkflowIr) -> String {
    let mut participants: Vec<(String, String)> = vec![];
    let mut body = String::new();
    let add = |id: String, label: String, ps: &mut Vec<(String, String)>| {
        if !ps.iter().any(|(i, _)| *i == id) {
            ps.push((id, label));
        }
    };
    let steps = |scope: Scope, indent: &str, body: &mut String, ps: &mut Vec<(String, String)>| {
        for s in scope_steps(wf, scope) {
            let mut ind = indent.to_string();
            if let Some(c) = &s.condition {
                let _ = writeln!(body, "{ind}opt when {}", seq_text(&pretty::expr(c)));
                ind.push_str("    ");
            }
            if let Some(f) = &s.for_each {
                let _ = writeln!(body, "{ind}loop for each {}", f.variable);
                ind.push_str("    ");
            }
            match &s.action {
                Action::Task(i) => {
                    let p = participant(&i.task);
                    add(p.clone(), p.clone(), ps);
                    let _ = writeln!(
                        body,
                        "{ind}W->>+{p}: {}: {}@{}",
                        s.id, i.task, i.contract_version
                    );
                    let out = wf
                        .contracts
                        .get(&i.task)
                        .map(|c| short_type(&c.output))
                        .unwrap_or_default();
                    let _ = writeln!(body, "{ind}{p}-->>-W: {}", seq_text(&out));
                }
                Action::Workflow(i) => {
                    let name = i.task.trim_start_matches(WORKFLOW_CONTRACT_PREFIX);
                    let p = format!("child_{}", participant(name));
                    add(p.clone(), format!("{name} (child workflow)"), ps);
                    let _ = writeln!(body, "{ind}W->>+{p}: {}: start {name}", s.id);
                    let out = wf
                        .contracts
                        .get(&i.task)
                        .map(|c| short_type(&c.output))
                        .unwrap_or_default();
                    let _ = writeln!(body, "{ind}{p}-->>-W: {}", seq_text(&out));
                }
                Action::Await { signal, timeout_ms } => {
                    add("Sender".into(), "Sender".into(), ps);
                    let t = timeout_ms
                        .map(|t| format!(" (timeout {})", super::duration(t)))
                        .unwrap_or_default();
                    let _ = writeln!(body, "{ind}Sender->>W: {}: signal {signal}{t}", s.id);
                }
                Action::Sleep { duration_ms } => {
                    let _ = writeln!(
                        body,
                        "{ind}Note over W: {}: sleep {}",
                        s.id,
                        super::duration(*duration_ms)
                    );
                }
                Action::Compute { .. } => {
                    let _ = writeln!(body, "{ind}Note over W: {}: compute", s.id);
                }
            }
            if s.for_each.is_some() {
                ind.truncate(ind.len() - 4);
                let _ = writeln!(body, "{ind}end");
            }
            if s.condition.is_some() {
                ind.truncate(ind.len() - 4);
                let _ = writeln!(body, "{ind}end");
            }
        }
    };
    steps(Scope::Main, "    ", &mut body, &mut participants);
    if let Some(c) = &wf.continue_as_new {
        let next = format!(
            "W->>W: continue_as_new: next run with {}",
            seq_text(&input_fields(&c.input))
        );
        match &c.condition {
            Some(e) => {
                let _ = writeln!(body, "    opt when {}", seq_text(&pretty::expr(e)));
                let _ = writeln!(body, "        {next}");
                body.push_str("    end\n");
            }
            None => {
                let _ = writeln!(body, "    {next}");
            }
        }
    }
    let comps: Vec<&StepIr> = wf
        .steps
        .iter()
        .filter(|s| s.compensation.is_some())
        .collect();
    if !comps.is_empty() {
        body.push_str("    opt compensation, in reverse order, after a failure or cancellation\n");
        for s in comps.iter().rev() {
            let c = s.compensation.as_ref().unwrap();
            let p = participant(&c.task);
            add(p.clone(), p.clone(), &mut participants);
            let _ = writeln!(
                body,
                "        W->>+{p}: undo {}: {}@{}",
                s.id, c.task, c.contract_version
            );
            let out = wf
                .contracts
                .get(&c.task)
                .map(|c| short_type(&c.output))
                .unwrap_or_default();
            let _ = writeln!(body, "        {p}-->>-W: {}", seq_text(&out));
        }
        body.push_str("    end\n");
    }
    for scope in scopes(wf).into_iter().skip(1) {
        let _ = writeln!(body, "    opt {}", scope.label());
        steps(scope, "        ", &mut body, &mut participants);
        body.push_str("    end\n");
    }
    let mut out = String::from("sequenceDiagram\n");
    for h in header(wf) {
        let _ = writeln!(out, "    %% {h}");
    }
    let _ = writeln!(out, "    participant W as {}", wf.name);
    for (id, label) in &participants {
        if id == label {
            let _ = writeln!(out, "    participant {id}");
        } else {
            let _ = writeln!(out, "    participant {id} as {label}");
        }
    }
    out.push_str(&body);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile_files;
    use std::path::PathBuf;

    fn example(name: &str) -> Vec<WorkflowIr> {
        let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples")
            .join(name);
        let c = compile_files(&[p]);
        assert!(!c.has_errors(), "{}", c.render_diagnostics(false));
        c.workflows
    }

    fn golden(name: &str, actual: &str) {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/golden")
            .join(name);
        if std::env::var_os("ORBIT_BLESS").is_some() {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, actual).unwrap();
        }
        let expected = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{}: {e} (run with ORBIT_BLESS=1)", path.display()));
        assert_eq!(
            actual, expected,
            "golden file {name} differs (ORBIT_BLESS=1 updates it)"
        );
    }

    #[test]
    fn checkout_golden_outputs() {
        let wfs = example("checkout.orbit");
        golden("checkout.mmd", &mermaid(&wfs[0]));
        golden("checkout.dot", &dot(&wfs[0]));
        golden("checkout.seq.mmd", &sequence(&wfs[0]));
    }

    #[test]
    fn approval_golden_outputs() {
        let wfs = example("approval.orbit");
        golden("approval.mmd", &mermaid(&wfs[0]));
        golden("approval.dot", &dot(&wfs[0]));
    }

    #[test]
    fn deterministic_and_escaped() {
        let wfs = example("approval.orbit");
        assert_eq!(mermaid(&wfs[0]), mermaid(&wfs[0].clone()));
        let m = mermaid(&wfs[0]);
        // `>=` in a condition must not break Mermaid's label syntax.
        assert!(!m.contains(">="), "{m}");
        assert!(m.contains("signal__review>\"signal review\"]:::signal"));
        assert!(m.contains("charge__undo([\"undo charge<br/><small>payments.refund@1.0.0</small>\"]):::compensation"));
        let d = dot(&wfs[0]);
        assert!(d.contains("\"charge\" -> \"charge__undo\" [style=dashed, label=\"compensate\"];"));
    }

    #[test]
    fn handlers_and_events_are_subgraphs() {
        let wfs = example("shipment.orbit");
        let m = mermaid(&wfs[0]);
        assert!(
            m.contains("subgraph on_event_location[\"on event location\"]"),
            "{m}"
        );
        let wfs = example("checkout.orbit");
        let m = mermaid(&wfs[0]);
        assert!(m.contains("subgraph on_failure[\"on failure\"]"));
        assert!(m.contains("subgraph on_cancel[\"on cancel\"]"));
        let d = dot(&wfs[0]);
        assert!(d.contains("subgraph \"cluster_on_failure\""));
    }

    #[test]
    fn subscription_golden_outputs() {
        let wfs = example("subscription.orbit");
        golden("subscription.mmd", &mermaid(&wfs[0]));
        golden("subscription.dot", &dot(&wfs[0]));
        golden("subscription.seq.mmd", &sequence(&wfs[0]));
    }

    #[test]
    fn continue_as_new_follows_the_final_steps_and_loops_back() {
        let wfs = example("subscription.orbit");
        let m = mermaid(&wfs[0]);
        assert!(
            m.contains("continue_as_new[[\"continue as new<br/><small>when input.period #lt; input.periods</small>"),
            "{m}"
        );
        assert!(m.contains("    next_period --> continue_as_new\n"), "{m}");
        assert!(
            m.contains("    continue_as_new -. next run .-> charge\n"),
            "{m}"
        );
        // The back edge comes after the node it starts from.
        assert!(m.find("continue_as_new[[").unwrap() < m.find("-. next run").unwrap());
        let d = dot(&wfs[0]);
        assert!(
            d.contains("\"continue_as_new\" -> \"charge\" [style=dashed, constraint=false, label=\"next run\"];"),
            "{d}"
        );
        let s = sequence(&wfs[0]);
        assert!(s.contains("    opt when input.period < input.periods\n        W->>W: continue_as_new: next run with { customer_id, amount, periods, period, charged }\n    end\n"), "{s}");

        // Without `when`: always; several first and final steps.
        let c = crate::compile_str(
            "loop.orbit",
            r#"
workflow Poll {
    version "1.0.0"
    input { cursor: Int = 0 }
    step a { compute input.cursor }
    step b { compute input.cursor + 1 }
    step c { compute a + 1 }
    continue_as_new with { cursor: c + b }
}
"#,
        );
        assert!(!c.has_errors(), "{}", c.render_diagnostics(false));
        let m = mermaid(&c.workflows[0]);
        assert!(
            m.contains("<small>always</small><br/><small>next input { cursor }</small>"),
            "{m}"
        );
        for edge in [
            "b --> continue_as_new",
            "c --> continue_as_new",
            "continue_as_new -. next run .-> a",
            "continue_as_new -. next run .-> b",
        ] {
            assert!(m.contains(edge), "{edge}: {m}");
        }
        assert!(!m.contains("a --> continue_as_new"), "{m}");
        let s = sequence(&c.workflows[0]);
        assert!(
            s.contains("\n    W->>W: continue_as_new: next run with { cursor }\n"),
            "{s}"
        );
        assert!(!s.contains("opt when"), "{s}");
        // Workflows without it are drawn as before.
        assert!(!mermaid(&example("approval.orbit")[0]).contains("continue_as_new"));
    }

    #[test]
    fn fan_out_and_children() {
        let wfs = example("fulfilment.orbit");
        let f = wfs.iter().find(|w| w.name == "Fulfilment").unwrap();
        let m = mermaid(f);
        assert!(m.contains("for each item (max 4 parallel)"), "{m}");
        assert!(m.contains("child workflow Shipping"));
        let s = sequence(f);
        assert!(s.contains("loop for each item"), "{s}");
        assert!(s.contains("participant child_Shipping as Shipping (child workflow)"));
    }
}
