//! `orbit docs`: reference documentation for workflows and task contracts,
//! as Markdown or as self-contained HTML pages.
//!
//! The pages are built from a small document model ([`Block`]) so that both
//! formats carry the same content. Inline text uses two pieces of markup only:
//! `` `code` `` spans and `[text](href)` links; doc comments are rendered as
//! paragraphs with the same inline markup.

use super::graph;
use super::pretty;
use super::schema;
use super::source::{SourceIndex, WorkflowSource};
use super::svg;
use super::{action_summary, retry_summary, scope_steps, scopes, timeout_summary, Scope};
use crate::Compilation;
use orbit_ir::{Action, TaskContract, Type, WorkflowIr, WORKFLOW_CONTRACT_PREFIX};
use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Markdown,
    Html,
}

impl Format {
    pub fn extension(self) -> &'static str {
        match self {
            Format::Markdown => "md",
            Format::Html => "html",
        }
    }
}

#[derive(Debug, Clone)]
pub enum Block {
    Heading(u8, String, Option<String>),
    Para(String),
    Table(Vec<String>, Vec<Vec<String>>),
    Code(String, String),
    /// A diagram: Mermaid source (Markdown) and an SVG rendering (HTML).
    Diagram {
        mermaid: String,
        svg: String,
    },
}

/// A generated page: file name and content.
pub struct Page {
    pub file: String,
    pub title: String,
    pub blocks: Vec<Block>,
}

/// Options for [`generate`].
#[derive(Default)]
pub struct Options {
    /// Link each workflow page to its JSON Schema files in `schemas/`.
    pub schema_links: bool,
}

/// One page per workflow, `contracts`, and `index`.
pub fn generate(c: &Compilation, opts: &Options) -> Vec<Page> {
    let idx = SourceIndex::new(c);
    let contracts = documented_contracts(c);
    let mut pages = vec![index_page(c, &contracts)];
    for wf in &c.workflows {
        pages.push(workflow_page(c, &idx, wf, opts));
    }
    pages.push(contracts_page(c, &idx, &contracts));
    pages
}

/// User contracts declared in the sources, and built-in contracts that a
/// workflow uses, by name.
pub fn documented_contracts(c: &Compilation) -> BTreeMap<String, TaskContract> {
    let mut out: BTreeMap<String, TaskContract> = c
        .contracts
        .values()
        .filter(|t| !t.builtin && !t.name.starts_with(WORKFLOW_CONTRACT_PREFIX))
        .map(|t| (t.name.clone(), t.clone()))
        .collect();
    for wf in &c.workflows {
        for t in wf.contracts.values() {
            if !t.name.starts_with(WORKFLOW_CONTRACT_PREFIX) {
                out.entry(t.name.clone()).or_insert_with(|| t.clone());
            }
        }
    }
    out
}

fn code(s: &str) -> String {
    if s.is_empty() {
        return String::new();
    }
    if s.contains('`') {
        format!("`` {s} ``")
    } else {
        format!("`{s}`")
    }
}

pub fn anchor(task: &str) -> String {
    format!(
        "task-{}",
        task.chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect::<String>()
    )
}

fn task_link(task: &str, ext: &str) -> String {
    format!("[`{task}`](contracts.{ext}#{})", anchor(task))
}

/// Field rows: (path, type, required, default, doc).
fn field_rows(
    t: &Type,
    prefix: &str,
    doc_key: &str,
    docs: &dyn Fn(&str) -> Option<String>,
    schemas: &HashMap<String, BTreeMap<String, Option<String>>>,
    out: &mut Vec<[String; 5]>,
) {
    let Type::Record { name, fields } = t else {
        return;
    };
    for f in fields {
        let path = if prefix.is_empty() {
            f.name.clone()
        } else {
            format!("{prefix}.{}", f.name)
        };
        let doc = match name.as_ref().and_then(|n| schemas.get(n)) {
            Some(fs) => fs.get(&f.name).cloned().flatten(),
            None => docs(&format!("{doc_key}.{}", f.name)),
        };
        let required = if f.required() { "yes" } else { "no" };
        let default = f
            .default
            .as_ref()
            .map(|d| code(&pretty::literal(d)))
            .unwrap_or_default();
        out.push([
            code(&path),
            code(&f.ty.to_string()),
            required.into(),
            default,
            doc.unwrap_or_default().replace('\n', " "),
        ]);
        // Descend into records, also inside lists, maps and optionals.
        let mut inner = &f.ty;
        let mut suffix = String::new();
        loop {
            match inner {
                Type::Optional { inner: i } => inner = i,
                Type::List { item } => {
                    suffix.push_str("[]");
                    inner = item
                }
                Type::Map { value, .. } => {
                    suffix.push_str("{}");
                    inner = value
                }
                _ => break,
            }
        }
        if matches!(inner, Type::Record { .. }) {
            field_rows(
                inner,
                &format!("{path}{suffix}"),
                &format!("{doc_key}.{}", f.name),
                docs,
                schemas,
                out,
            );
        }
    }
}

fn fields_table(
    t: &Type,
    doc_key: &str,
    docs: &dyn Fn(&str) -> Option<String>,
    schemas: &HashMap<String, BTreeMap<String, Option<String>>>,
    with_required: bool,
) -> Block {
    let mut rows = vec![];
    field_rows(t, "", doc_key, docs, schemas, &mut rows);
    if rows.is_empty() {
        return Block::Para(format!("No fields (`{t}`)."));
    }
    if with_required {
        Block::Table(
            vec![
                "Field".into(),
                "Type".into(),
                "Required".into(),
                "Default".into(),
                "Description".into(),
            ],
            rows.into_iter().map(|r| r.to_vec()).collect(),
        )
    } else {
        Block::Table(
            vec!["Field".into(), "Type".into(), "Description".into()],
            rows.into_iter()
                .map(|[a, b, _, _, e]| vec![a, b, e])
                .collect(),
        )
    }
}

fn schema_docs(idx: &SourceIndex) -> HashMap<String, BTreeMap<String, Option<String>>> {
    idx.schemas
        .iter()
        .map(|(k, (_, fs))| {
            (
                k.clone(),
                fs.iter().map(|(f, i)| (f.clone(), i.doc.clone())).collect(),
            )
        })
        .collect()
}

fn index_page(c: &Compilation, contracts: &BTreeMap<String, TaskContract>) -> Page {
    let ext = "{ext}";
    let mut blocks = vec![Block::Heading(1, "Workflow reference".into(), None)];
    blocks.push(Block::Para(format!(
        "Generated by `orbit docs` (compiler {}) from the compiled definitions.",
        crate::COMPILER_VERSION
    )));
    blocks.push(Block::Heading(2, "Workflows".into(), None));
    if c.workflows.is_empty() {
        blocks.push(Block::Para("No workflows.".into()));
    } else {
        blocks.push(Block::Table(
            vec![
                "Workflow".into(),
                "Version".into(),
                "Steps".into(),
                "Description".into(),
            ],
            c.workflows
                .iter()
                .map(|w| {
                    vec![
                        format!("[`{}`]({}.{ext})", w.name, w.name),
                        code(&w.version),
                        w.steps.len().to_string(),
                        w.description.clone().unwrap_or_default(),
                    ]
                })
                .collect(),
        ));
    }
    blocks.push(Block::Heading(2, "Task contracts".into(), None));
    blocks.push(Block::Table(
        vec!["Task".into(), "Version".into(), "Compensated by".into()],
        contracts
            .values()
            .map(|t| {
                vec![
                    task_link(&t.name, ext),
                    code(&t.version),
                    t.compensated_by.as_deref().map(code).unwrap_or_default(),
                ]
            })
            .collect(),
    ));
    Page {
        file: "index".into(),
        title: "Workflow reference".into(),
        blocks,
    }
}

fn step_table(wf: &WorkflowIr, src: Option<&WorkflowSource>, scope: Scope) -> Block {
    let ext = "{ext}";
    let label = scope.label();
    let rows = scope_steps(wf, scope)
        .iter()
        .map(|s| {
            let action = match &s.action {
                Action::Task(i) => {
                    let mut a = format!("{}@{}", task_link(&i.task, ext), i.contract_version);
                    if let Some(l) = &i.language {
                        a.push_str(&format!(" using `{l}`"));
                    }
                    a
                }
                Action::Workflow(i) => format!(
                    "child workflow [`{0}`]({0}.{ext})",
                    i.task.trim_start_matches(WORKFLOW_CONTRACT_PREFIX)
                ),
                Action::Compute { expr } => format!("compute {}", code(&pretty::expr(expr))),
                _ => code(&action_summary(s)),
            };
            let action = match &s.for_each {
                Some(f) => {
                    let par = f
                        .max_parallel
                        .map(|n| format!(", at most {n} in parallel"))
                        .unwrap_or_default();
                    format!(
                        "{action}, for each `{}` in {}{par}",
                        f.variable,
                        code(&pretty::expr(&f.items))
                    )
                }
                None => action,
            };
            let comp = s
                .compensation
                .as_ref()
                .map(|c| format!("{}@{}", task_link(&c.task, ext), c.contract_version))
                .unwrap_or_default();
            let retry = s
                .invocation()
                .and_then(|i| retry_summary(&i.retry))
                .unwrap_or_default();
            let mut timeouts = timeout_summary(s).unwrap_or_default();
            if s.invocation().is_some_and(|i| i.idempotency_key.is_some()) {
                if !timeouts.is_empty() {
                    timeouts.push_str("; ");
                }
                timeouts.push_str("idempotency key ");
                timeouts.push_str(&code(&pretty::expr(
                    s.invocation().unwrap().idempotency_key.as_ref().unwrap(),
                )));
            }
            let doc = src
                .and_then(|w| w.steps.get(&(label.clone(), s.id.clone())))
                .and_then(|i| i.doc.clone())
                .unwrap_or_default()
                .replace('\n', " ");
            vec![
                code(&s.id),
                action,
                s.dependencies
                    .iter()
                    .map(|d| code(d))
                    .collect::<Vec<_>>()
                    .join(", "),
                s.condition
                    .as_ref()
                    .map(|c| code(&pretty::expr(c)))
                    .unwrap_or_default(),
                retry,
                timeouts,
                comp,
                doc,
            ]
        })
        .collect();
    Block::Table(
        [
            "Step",
            "Action",
            "After",
            "When",
            "Retry",
            "Timeouts",
            "Compensation",
            "Description",
        ]
        .map(String::from)
        .to_vec(),
        rows,
    )
}

fn workflow_page(c: &Compilation, idx: &SourceIndex, wf: &WorkflowIr, opts: &Options) -> Page {
    let ext = "{ext}";
    let src = idx.workflows.get(&wf.name);
    let field_doc = |k: &str| -> Option<String> {
        src.and_then(|s| s.fields.get(k))
            .and_then(|i| i.doc.clone())
    };
    let sdocs = schema_docs(idx);
    let mut b = vec![Block::Heading(1, format!("Workflow `{}`", wf.name), None)];
    if let Some(d) = src.and_then(|s| s.name.doc.clone()) {
        for p in d.split("\n\n") {
            b.push(Block::Para(p.replace('\n', " ")));
        }
    }
    if let Some(d) = &wf.description {
        b.push(Block::Para(d.clone()));
    }
    let mut facts = vec![
        vec!["Version".into(), code(&wf.version)],
        vec!["Definition hash".into(), code(&wf.definition_hash)],
        vec!["IR version".into(), code(&wf.ir_version)],
    ];
    if let Some(s) = src {
        let loc = c.sources.location(s.name.span);
        facts.push(vec![
            "Source".into(),
            code(&format!("{}:{}", loc.file, loc.line)),
        ]);
    }
    facts.push(vec![
        "Workflow timeout".into(),
        wf.timeout_ms
            .map(|t| code(&super::duration(t)))
            .unwrap_or_else(|| "none".into()),
    ]);
    facts.push(vec![
        "Limits".into(),
        wf.limits
            .as_ref()
            .map(|l| code(&graph::limits_summary(l)))
            .unwrap_or_else(|| "server defaults".into()),
    ]);
    b.push(Block::Table(vec!["".into(), "".into()], facts));

    b.push(Block::Heading(2, "Input".into(), None));
    b.push(fields_table(&wf.input, "input", &field_doc, &sdocs, true));
    b.push(Block::Heading(2, "Output".into(), None));
    b.push(fields_table(
        &wf.output, "output", &field_doc, &sdocs, false,
    ));
    if let Some(r) = &wf.result {
        b.push(Block::Code(
            "orbit".into(),
            format!("return {}", pretty::expr(r)),
        ));
    }

    if !wf.signals.is_empty() {
        b.push(Block::Heading(2, "Signals".into(), None));
        for (name, t) in &wf.signals {
            b.push(Block::Heading(3, format!("Signal `{name}`"), None));
            if let Some(d) = src
                .and_then(|s| s.signals.get(name))
                .and_then(|i| i.doc.clone())
            {
                b.push(Block::Para(d.replace('\n', " ")));
            }
            let awaited: Vec<String> = scopes(wf)
                .into_iter()
                .flat_map(|sc| {
                    scope_steps(wf, sc)
                        .iter()
                        .filter(
                            |s| matches!(&s.action, Action::Await { signal, .. } if signal == name),
                        )
                        .map(move |s| code(&sc.qualify(&s.id)))
                })
                .collect();
            let mut used = awaited.join(", ");
            if wf.events.contains_key(name) {
                if !used.is_empty() {
                    used.push_str("; ");
                }
                used.push_str(&format!("`on event {name}`"));
            }
            if !used.is_empty() {
                b.push(Block::Para(format!("Received by {used}.")));
            }
            b.push(fields_table(
                t,
                &format!("signal {name}"),
                &field_doc,
                &sdocs,
                true,
            ));
        }
    }
    if !wf.queries.is_empty() {
        b.push(Block::Heading(2, "Queries".into(), None));
        b.push(Block::Table(
            vec![
                "Query".into(),
                "Result type".into(),
                "Expression".into(),
                "Description".into(),
            ],
            wf.queries
                .iter()
                .map(|(n, q)| {
                    vec![
                        code(n),
                        code(&q.output_type.to_string()),
                        code(&pretty::expr(&q.expr)),
                        src.and_then(|s| s.queries.get(n))
                            .and_then(|i| i.doc.clone())
                            .unwrap_or_default()
                            .replace('\n', " "),
                    ]
                })
                .collect(),
        ));
    }

    b.push(Block::Heading(2, "Steps".into(), None));
    b.push(Block::Para(
        "In execution order. `After` lists direct dependencies, including those implied by reading another step's output.".into(),
    ));
    b.push(step_table(wf, src, Scope::Main));
    let handler_scopes: Vec<Scope> = scopes(wf)
        .into_iter()
        .filter(|s| matches!(s, Scope::Handler(_)))
        .collect();
    if !handler_scopes.is_empty() {
        b.push(Block::Heading(2, "Outcome handlers".into(), None));
        b.push(Block::Para(
            "Run after compensation has finished, when the execution fails, is cancelled or times out.".into(),
        ));
        for sc in handler_scopes {
            b.push(Block::Heading(3, format!("`{}`", sc.label()), None));
            b.push(step_table(wf, src, sc));
        }
    }
    if !wf.events.is_empty() {
        b.push(Block::Heading(2, "Event handlers".into(), None));
        b.push(Block::Para(
            "Run once per received signal of that name, alongside the main steps.".into(),
        ));
        for sig in wf.events.keys() {
            let sc = Scope::Event(sig);
            b.push(Block::Heading(3, format!("`{}`", sc.label()), None));
            b.push(step_table(wf, src, sc));
        }
    }

    b.push(Block::Heading(2, "Diagram".into(), None));
    b.push(Block::Diagram {
        mermaid: graph::mermaid(wf),
        svg: svg::svg(wf),
    });

    b.push(Block::Heading(2, "Task contracts used".into(), None));
    let used: Vec<&TaskContract> = wf
        .contracts
        .values()
        .filter(|t| !t.name.starts_with(WORKFLOW_CONTRACT_PREFIX))
        .collect();
    if used.is_empty() {
        b.push(Block::Para("None.".into()));
    } else {
        b.push(Block::Table(
            vec![
                "Task".into(),
                "Version".into(),
                "Input".into(),
                "Output".into(),
                "Compensated by".into(),
            ],
            used.iter()
                .map(|t| {
                    vec![
                        task_link(&t.name, ext),
                        code(&t.version),
                        code(&t.input.to_string()),
                        code(&t.output.to_string()),
                        t.compensated_by.as_deref().map(code).unwrap_or_default(),
                    ]
                })
                .collect(),
        ));
    }
    let children: Vec<String> = wf
        .contracts
        .values()
        .filter_map(|t| {
            t.name
                .strip_prefix(WORKFLOW_CONTRACT_PREFIX)
                .map(|n| format!("[`{n}`]({n}.{ext})@{}", t.version))
        })
        .collect();
    if !children.is_empty() {
        b.push(Block::Para(format!(
            "Child workflows: {}.",
            children.join(", ")
        )));
    }
    if opts.schema_links {
        b.push(Block::Heading(2, "JSON Schema".into(), None));
        b.push(Block::Table(
            vec!["Payload".into(), "Schema".into()],
            schema::workflow_schemas(wf)
                .keys()
                .map(|k| {
                    let what = k
                        .strip_prefix(&format!("workflow.{}.", wf.name))
                        .unwrap_or(k)
                        .replace('.', " ");
                    vec![
                        what,
                        format!("[`{k}.schema.json`](schemas/{k}.schema.json)"),
                    ]
                })
                .collect(),
        ));
    }
    Page {
        file: wf.name.clone(),
        title: format!("Workflow {}", wf.name),
        blocks: b,
    }
}

fn contracts_page(
    c: &Compilation,
    idx: &SourceIndex,
    contracts: &BTreeMap<String, TaskContract>,
) -> Page {
    let ext = "{ext}";
    let sdocs = schema_docs(idx);
    let mut b = vec![Block::Heading(1, "Task contracts".into(), None)];
    b.push(Block::Para(
        "The typed interfaces that workers implement. Every step that calls a task is checked against its contract.".into(),
    ));
    for t in contracts.values() {
        b.push(Block::Heading(2, code(&t.name), Some(anchor(&t.name))));
        let src = idx.tasks.get(&t.name);
        if let Some(d) = src.and_then(|s| s.name.doc.clone()) {
            for p in d.split("\n\n") {
                b.push(Block::Para(p.replace('\n', " ")));
            }
        }
        if let Some(d) = src.and_then(|s| s.description.clone()) {
            b.push(Block::Para(d));
        }
        let users: Vec<String> = c
            .workflows
            .iter()
            .filter(|w| w.contracts.contains_key(&t.name))
            .map(|w| format!("[`{0}`]({0}.{ext})", w.name))
            .collect();
        let mut facts = vec![vec!["Version".into(), code(&t.version)]];
        if let Some(by) = &t.compensated_by {
            facts.push(vec!["Compensated by".into(), task_link(by, ext)]);
        }
        if t.builtin {
            facts.push(vec!["Built-in".into(), "executed by the runtime".into()]);
        }
        if let Some(s) = src {
            let loc = c.sources.location(s.name.span);
            facts.push(vec![
                "Source".into(),
                code(&format!("{}:{}", loc.file, loc.line)),
            ]);
        }
        facts.push(vec![
            "Used by".into(),
            if users.is_empty() {
                "no workflow in this set".into()
            } else {
                users.join(", ")
            },
        ]);
        b.push(Block::Table(vec!["".into(), "".into()], facts));
        let task_doc = |k: &str| -> Option<String> {
            src.and_then(|s| s.fields.get(k))
                .and_then(|i| i.doc.clone())
        };
        b.push(Block::Heading(3, "Input".into(), None));
        b.push(fields_table(&t.input, "input", &task_doc, &sdocs, true));
        b.push(Block::Heading(3, "Output".into(), None));
        b.push(fields_table(&t.output, "output", &task_doc, &sdocs, false));
    }
    Page {
        file: "contracts".into(),
        title: "Task contracts".into(),
        blocks: b,
    }
}

// ----------------------------------------------------------------- rendering

fn md_cell(s: &str) -> String {
    s.replace('|', "\\|").replace('\n', " ")
}

/// Render a page as Markdown.
pub fn markdown(page: &Page) -> String {
    let mut out = String::new();
    for block in &page.blocks {
        match block {
            Block::Heading(level, text, anchor) => {
                if let Some(a) = anchor {
                    let _ = writeln!(out, "<a id=\"{a}\"></a>\n");
                }
                let _ = writeln!(out, "{} {text}\n", "#".repeat(*level as usize));
            }
            Block::Para(p) => {
                let _ = writeln!(out, "{p}\n");
            }
            Block::Table(head, rows) => {
                let _ = writeln!(
                    out,
                    "| {} |",
                    head.iter()
                        .map(|h| md_cell(h))
                        .collect::<Vec<_>>()
                        .join(" | ")
                );
                let _ = writeln!(out, "|{}", "---|".repeat(head.len()));
                for r in rows {
                    let _ = writeln!(
                        out,
                        "| {} |",
                        r.iter().map(|c| md_cell(c)).collect::<Vec<_>>().join(" | ")
                    );
                }
                out.push('\n');
            }
            Block::Code(lang, text) => {
                let _ = writeln!(out, "```{lang}\n{}\n```\n", text.trim_end());
            }
            Block::Diagram { mermaid, .. } => {
                let _ = writeln!(out, "```mermaid\n{}```\n", mermaid);
            }
        }
    }
    out.trim_end().to_string().replace("{ext}", "md") + "\n"
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Inline markup: `` `code` `` and `[text](href)`, everything else escaped.
fn inline(s: &str) -> String {
    let mut out = String::new();
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '`' => {
                // `` code `` (double) or `code`.
                let double = chars.get(i + 1) == Some(&'`');
                let (open, close): (usize, &[char]) = if double {
                    (i + 2, &['`', '`'])
                } else {
                    (i + 1, &['`'])
                };
                let end = (open..chars.len())
                    .find(|&j| chars[j..].starts_with(close))
                    .unwrap_or(chars.len());
                let text: String = chars[open..end].iter().collect();
                let text = if double {
                    text.trim().to_string()
                } else {
                    text
                };
                let _ = write!(out, "<code>{}</code>", html_escape(&text));
                i = (end + close.len()).min(chars.len());
            }
            '[' => {
                let close = (i + 1..chars.len()).find(|&j| chars[j] == ']');
                match close {
                    Some(c) if chars.get(c + 1) == Some(&'(') => {
                        let end = (c + 2..chars.len()).find(|&j| chars[j] == ')');
                        if let Some(e) = end {
                            let text: String = chars[i + 1..c].iter().collect();
                            let href: String = chars[c + 2..e].iter().collect();
                            let _ = write!(
                                out,
                                "<a href=\"{}\">{}</a>",
                                html_escape(&href),
                                inline(&text)
                            );
                            i = e + 1;
                        } else {
                            out.push('[');
                            i += 1;
                        }
                    }
                    _ => {
                        out.push('[');
                        i += 1;
                    }
                }
            }
            c => {
                out.push_str(&html_escape(&c.to_string()));
                i += 1;
            }
        }
    }
    out
}

const CSS: &str = r#"
:root {
  --bg: #faf9f7; --band: #f3f1ed; --ink: #1c1917; --muted: #57534e; --faint: #a8a29e;
  --line: #e2dfda; --accent: #c2410c; --accent-ink: #9a3412; --code-bg: #f3f1ed;
  --sans: system-ui, -apple-system, "Segoe UI", Roboto, sans-serif;
  --mono: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
}
@media (prefers-color-scheme: dark) {
  :root { --bg: #0f0d0c; --band: #161412; --ink: #ece9e4; --muted: #a8a29e; --faint: #6b6560;
    --line: #2a2623; --accent: #fb923c; --accent-ink: #fdba74; --code-bg: #1a1715; }
}
* { box-sizing: border-box; }
body { margin: 0; background: var(--bg); color: var(--ink); font: 400 16px/1.6 var(--sans); }
main { max-width: 1100px; margin: 0 auto; padding: 32px 28px 64px; }
nav { font-size: 14px; color: var(--muted); margin-bottom: 24px; }
a { color: var(--accent-ink); text-decoration: none; }
a:hover { text-decoration: underline; }
h1 { font-size: 30px; line-height: 1.2; margin: 0 0 16px; font-weight: 600; }
h2 { font-size: 21px; margin: 40px 0 12px; padding-bottom: 6px; border-bottom: 1px solid var(--line); font-weight: 600; }
h3 { font-size: 17px; margin: 28px 0 10px; font-weight: 600; }
p { margin: 0 0 14px; }
code { font-family: var(--mono); font-size: 0.88em; background: var(--code-bg); padding: 1px 4px; border-radius: 3px; }
pre { background: var(--code-bg); padding: 12px 14px; border-radius: 6px; overflow-x: auto; font: 13px/1.5 var(--mono); }
pre code { background: none; padding: 0; }
.table { overflow-x: auto; margin: 0 0 18px; }
table { border-collapse: collapse; font-size: 14px; width: 100%; }
th, td { text-align: left; vertical-align: top; padding: 7px 10px; border-bottom: 1px solid var(--line); }
th { color: var(--muted); font-weight: 600; }
table.facts td:first-child { color: var(--muted); white-space: nowrap; width: 1%; }
.diagram { overflow-x: auto; background: var(--band); border-radius: 6px; padding: 12px; margin-bottom: 12px; }
details { font-size: 14px; color: var(--muted); }
footer { margin-top: 48px; font-size: 13px; color: var(--faint); }
"#;

/// Render a page as a self-contained HTML document (inline CSS and SVG, no
/// external assets).
pub fn html(page: &Page) -> String {
    let mut body = String::new();
    for block in &page.blocks {
        match block {
            Block::Heading(level, text, anchor) => {
                let id = anchor
                    .as_ref()
                    .map(|a| format!(" id=\"{a}\""))
                    .unwrap_or_default();
                let _ = writeln!(body, "<h{level}{id}>{}</h{level}>", inline(text));
            }
            Block::Para(p) => {
                let _ = writeln!(body, "<p>{}</p>", inline(p));
            }
            Block::Table(head, rows) => {
                let facts = head.iter().all(|h| h.is_empty());
                let class = if facts { " class=\"facts\"" } else { "" };
                let _ = writeln!(body, "<div class=\"table\"><table{class}>");
                if !facts {
                    let _ = writeln!(
                        body,
                        "<thead><tr>{}</tr></thead>",
                        head.iter()
                            .map(|h| format!("<th>{}</th>", inline(h)))
                            .collect::<String>()
                    );
                }
                body.push_str("<tbody>\n");
                for r in rows {
                    let _ = writeln!(
                        body,
                        "<tr>{}</tr>",
                        r.iter()
                            .map(|c| format!("<td>{}</td>", inline(c)))
                            .collect::<String>()
                    );
                }
                body.push_str("</tbody></table></div>\n");
            }
            Block::Code(_, text) => {
                let _ = writeln!(
                    body,
                    "<pre><code>{}</code></pre>",
                    html_escape(text.trim_end())
                );
            }
            Block::Diagram { mermaid, svg } => {
                let _ = writeln!(body, "<div class=\"diagram\">\n{svg}</div>");
                let _ = writeln!(
                    body,
                    "<details><summary>Mermaid source</summary>\n<pre><code>{}</code></pre></details>",
                    html_escape(mermaid)
                );
            }
        }
    }
    let nav = if page.file == "index" {
        String::new()
    } else {
        "<nav><a href=\"index.html\">Workflow reference</a></nav>\n".to_string()
    };
    let out = format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n<meta name=\"generator\" content=\"orbit docs {}\">\n<title>{}</title>\n<style>{CSS}</style>\n</head>\n<body>\n<main>\n{nav}{body}<footer>Generated by <code>orbit docs</code> from the compiled definitions.</footer>\n</main>\n</body>\n</html>\n",
        crate::COMPILER_VERSION,
        html_escape(&page.title)
    );
    out.replace("{ext}", "html")
}

/// Render a page in the given format.
pub fn render(page: &Page, format: Format) -> String {
    match format {
        Format::Markdown => markdown(page),
        Format::Html => html(page),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile_files;
    use std::path::PathBuf;

    fn compile(name: &str) -> Compilation {
        let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples")
            .join(name);
        let c = compile_files(&[p]);
        assert!(!c.has_errors(), "{}", c.render_diagnostics(false));
        c
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
    fn approval_markdown_snapshot() {
        let c = compile("approval.orbit");
        let pages = generate(&c, &Options::default());
        let names: Vec<&str> = pages.iter().map(|p| p.file.as_str()).collect();
        assert_eq!(names, ["index", "OrderApproval", "contracts"]);
        let mut md = markdown(&pages[1]);
        // The source path depends on where the tests run from.
        let start = md.find("| Source | `").unwrap();
        let end = start + md[start..].find('\n').unwrap();
        md.replace_range(start..end, "| Source | `examples/approval.orbit:12` |");
        golden("OrderApproval.md", &md);
        assert!(md.contains("```mermaid\nflowchart TD\n"));
        let contracts = markdown(&pages[2]);
        assert!(contracts.contains("<a id=\"task-payments-charge\"></a>"));
        assert!(contracts.contains("[`OrderApproval`](OrderApproval.md)"));
    }

    #[test]
    fn html_is_self_contained() {
        let c = compile("checkout.orbit");
        let pages = generate(&c, &Options { schema_links: true });
        let h = html(&pages[1]);
        assert!(h.starts_with("<!doctype html>"));
        assert!(h.contains("<svg xmlns=\"http://www.w3.org/2000/svg\""));
        assert!(!h.contains("<script"));
        assert!(!h.contains("<link"));
        assert!(!h.contains("src=\""));
        assert!(!h.contains("{ext}"));
        assert!(h.contains("href=\"contracts.html#task-payments-charge\""));
        assert!(h.contains("schemas/workflow.Checkout.input.schema.json"));
    }

    #[test]
    fn inline_markup() {
        assert_eq!(
            inline("a `b<c>` [`x`](y.html#z) & d"),
            "a <code>b&lt;c&gt;</code> <a href=\"y.html#z\"><code>x</code></a> &amp; d"
        );
        assert_eq!(inline("`` a`b ``"), "<code>a`b</code>");
        assert_eq!(inline("[not a link"), "[not a link");
    }

    #[test]
    fn doc_comments_are_rendered() {
        let c = crate::compile_str(
            "d.orbit",
            r#"
/// Moves money.
task "pay.go" {
    input {
        /// In cents.
        amount: Int
    }
    output { ok: Bool }
}

/// Pays.
///
/// Second paragraph.
workflow Pay {
    version "1.0.0"
    input {
        /// What to pay.
        amount: Int
    }
    /// Do it.
    step go { worker "pay.go" with { amount: input.amount } }
}
"#,
        );
        assert!(!c.has_errors(), "{}", c.render_diagnostics(false));
        let pages = generate(&c, &Options::default());
        let wf = markdown(&pages[1]);
        assert!(wf.contains("\nPays.\n\nSecond paragraph.\n"), "{wf}");
        assert!(
            wf.contains("| `amount` | `Int` | yes |  | What to pay. |"),
            "{wf}"
        );
        assert!(wf.contains("| Do it. |"), "{wf}");
        let tasks = markdown(&pages[2]);
        assert!(tasks.contains("Moves money."), "{tasks}");
        assert!(tasks.contains("| In cents. |"), "{tasks}");
    }
}
