//! Source positions and `///` doc comments for compiled declarations.
//!
//! The compiler does not keep the syntax tree, so this module re-parses the
//! files of a [`Compilation`] (parsing is cheap and deterministic) and records,
//! by name, where each workflow, step, signal, query, field, task and schema
//! is declared, together with its doc comment.
//!
//! A doc comment is one or more `///` line comments, each alone on its line,
//! directly above the declaration (no blank line in between). The declaration
//! must start its own line: in `input { a: String, b: Int }` the fields have no
//! doc comments. `////` and longer runs of slashes are ordinary comments. Doc
//! comments are plain `//` comments to the lexer, parser and formatter, so
//! they never change a definition hash and `orbit fmt` keeps them as written.

use crate::Compilation;
use orbit_syntax::ast::{self, Declaration};
use orbit_syntax::lexer::tokenize_with_comments;
use orbit_syntax::{parse, FileId, SourceMap, Span};
use std::collections::{BTreeMap, HashMap};

/// Where something is declared, and its doc comment.
#[derive(Debug, Clone, Default)]
pub struct Item {
    pub span: Span,
    pub doc: Option<String>,
}

/// Declarations of one workflow.
#[derive(Debug, Clone, Default)]
pub struct WorkflowSource {
    pub file: FileId,
    /// The workflow name.
    pub name: Item,
    /// Keyed by scope label (`""`, `on failure`, `on event location`), then step.
    pub steps: BTreeMap<(String, String), Item>,
    pub signals: BTreeMap<String, Item>,
    pub queries: BTreeMap<String, Item>,
    /// Fields of `input`, `output` and signals, keyed by `input.a.b`,
    /// `output.x`, `signal review.approved`.
    pub fields: BTreeMap<String, Item>,
    /// Steps whose `///` doc comment block, or the comment block above them,
    /// contains the given line comments (text after `//`, trimmed); used for
    /// policy waivers. Keyed like `steps`.
    pub step_comments: BTreeMap<(String, String), Vec<(String, Span)>>,
    /// Comments directly above the workflow declaration.
    pub comments: Vec<(String, Span)>,
}

/// A task contract declaration.
#[derive(Debug, Clone, Default)]
pub struct TaskSource {
    pub name: Item,
    /// The contract's `description "..."` member.
    pub description: Option<String>,
    /// Fields of an inline `input { }` / `output { }` record, keyed `input.a`.
    pub fields: BTreeMap<String, Item>,
}

/// Index of the user files in a compilation (the prelude is skipped).
#[derive(Debug, Clone, Default)]
pub struct SourceIndex {
    pub workflows: HashMap<String, WorkflowSource>,
    pub tasks: HashMap<String, TaskSource>,
    /// Schemas by name, with their fields keyed by field name.
    pub schemas: HashMap<String, (Item, BTreeMap<String, Item>)>,
}

struct FileCtx<'a> {
    text: &'a str,
    line_starts: Vec<usize>,
    /// Line (0-based) → (comment text after `//`, span) for line comments
    /// that are the first thing on their line.
    line_comments: HashMap<usize, (String, Span)>,
}

impl FileCtx<'_> {
    fn line_of(&self, offset: usize) -> usize {
        match self.line_starts.binary_search(&offset) {
            Ok(l) => l,
            Err(l) => l - 1,
        }
    }

    fn starts_line(&self, offset: usize) -> bool {
        let line = self.line_of(offset);
        self.text[self.line_starts[line]..offset].trim().is_empty()
    }

    /// The comment lines directly above the line holding `offset`.
    fn comments_above(&self, offset: usize) -> Vec<(String, Span)> {
        if !self.starts_line(offset) {
            return vec![];
        }
        let mut line = self.line_of(offset);
        let mut out = vec![];
        while line > 0 {
            line -= 1;
            match self.line_comments.get(&line) {
                Some(c) => out.push(c.clone()),
                None => break,
            }
        }
        out.reverse();
        out
    }

    fn doc(&self, offset: usize) -> Option<String> {
        let above = self.comments_above(offset);
        // Only the `///` lines immediately above count; a plain comment
        // between ends the doc block.
        let mut lines = vec![];
        for (text, _) in above.iter().rev() {
            match text.strip_prefix('/') {
                Some(rest) if !rest.starts_with('/') => {
                    lines.push(rest.strip_prefix(' ').unwrap_or(rest).trim_end())
                }
                _ => break,
            }
        }
        if lines.is_empty() {
            return None;
        }
        lines.reverse();
        Some(lines.join("\n").trim().to_string()).filter(|s| !s.is_empty())
    }

    fn item(&self, span: Span) -> Item {
        Item {
            span,
            doc: self.doc(span.start as usize),
        }
    }
}

impl SourceIndex {
    pub fn new(c: &Compilation) -> SourceIndex {
        Self::from_sources(&c.sources)
    }

    pub fn from_sources(sources: &SourceMap) -> SourceIndex {
        let mut idx = SourceIndex::default();
        for (id, file) in sources.files() {
            if file.name == "<prelude>" {
                continue;
            }
            let (_, comments, _) = tokenize_with_comments(id, &file.text);
            let mut line_starts = vec![0];
            for (i, b) in file.text.bytes().enumerate() {
                if b == b'\n' {
                    line_starts.push(i + 1);
                }
            }
            let mut ctx = FileCtx {
                text: &file.text,
                line_starts,
                line_comments: HashMap::new(),
            };
            for c in comments.iter().filter(|c| !c.block) {
                let start = c.span.start as usize;
                if ctx.starts_line(start) {
                    let text = file.text[start..c.span.end as usize]
                        .trim_start_matches("//")
                        .to_string();
                    ctx.line_comments.insert(ctx.line_of(start), (text, c.span));
                }
            }
            let program = parse(id, &file.text).program;
            for d in &program.declarations {
                match d {
                    Declaration::Workflow(wf) => {
                        idx.workflows
                            .insert(wf.name.name.clone(), workflow(&ctx, id, wf));
                    }
                    Declaration::Task(t) => {
                        let mut fields = BTreeMap::new();
                        for (prefix, ty) in [("input", &t.input), ("output", &t.output)] {
                            if let Some(ast::TypeExpr::Record(fs, _)) = ty {
                                record_fields(&ctx, prefix, fs, &mut fields);
                            }
                        }
                        idx.tasks.insert(
                            t.name.clone(),
                            TaskSource {
                                name: ctx.item(t.span),
                                description: t.description.clone(),
                                fields,
                            },
                        );
                    }
                    Declaration::Schema(s) => {
                        let fields = s
                            .fields
                            .iter()
                            .map(|f| (f.name.name.clone(), ctx.item(f.span)))
                            .collect();
                        idx.schemas
                            .insert(s.name.name.clone(), (ctx.item(s.span), fields));
                    }
                    Declaration::Import(_) | Declaration::Test(_) => {}
                }
            }
        }
        idx
    }

    /// The source span of a step, if the step is declared in a user file.
    pub fn step(&self, workflow: &str, scope: &str, step: &str) -> Option<&Item> {
        self.workflows
            .get(workflow)?
            .steps
            .get(&(scope.to_string(), step.to_string()))
    }
}

fn record_fields(
    ctx: &FileCtx,
    prefix: &str,
    fields: &[ast::Field],
    out: &mut BTreeMap<String, Item>,
) {
    for f in fields {
        let key = format!("{prefix}.{}", f.name.name);
        out.insert(key.clone(), ctx.item(f.span));
        if let ast::TypeExpr::Record(inner, _) = &f.ty {
            record_fields(ctx, &key, inner, out);
        }
    }
}

fn workflow(ctx: &FileCtx, file: FileId, wf: &ast::Workflow) -> WorkflowSource {
    let mut out = WorkflowSource {
        file,
        name: Item {
            span: wf.name.span,
            doc: ctx.doc(wf.span.start as usize),
        },
        comments: ctx.comments_above(wf.span.start as usize),
        ..Default::default()
    };
    let add_steps = |scope: String, steps: &[ast::Step], out: &mut WorkflowSource| {
        for s in steps {
            let key = (scope.clone(), s.name.name.clone());
            out.steps.insert(
                key.clone(),
                Item {
                    span: s.name.span,
                    doc: ctx.doc(s.span.start as usize),
                },
            );
            out.step_comments
                .insert(key, ctx.comments_above(s.span.start as usize));
        }
    };
    add_steps(String::new(), &wf.steps, &mut out);
    for h in &wf.handlers {
        let scope = match &h.event {
            Some(e) => format!("on event {}", e.name),
            None => format!("on {}", h.kind.name),
        };
        add_steps(scope, &h.steps, &mut out);
    }
    for s in &wf.signals {
        out.signals.insert(
            s.name.name.clone(),
            Item {
                span: s.name.span,
                doc: ctx.doc(s.span.start as usize),
            },
        );
        record_fields(
            ctx,
            &format!("signal {}", s.name.name),
            &s.fields,
            &mut out.fields,
        );
    }
    for q in &wf.queries {
        out.queries.insert(
            q.name.name.clone(),
            Item {
                span: q.name.span,
                doc: ctx.doc(q.span.start as usize),
            },
        );
    }
    record_fields(ctx, "input", &wf.input, &mut out.fields);
    if let Some((fs, _)) = &wf.output {
        record_fields(ctx, "output", fs, &mut out.fields);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile_str;

    const SRC: &str = r#"/// Charges a card.
/// Second line.
task "pay.charge" {
    input {
        /// Amount in cents.
        amount: Int
    }
    output { id: String }
}

// A plain comment, not documentation.
/// Takes payments.
workflow Pay {
    version "1.0.0"
    input {
        /// The order.
        order_id: String
        //// not a doc comment
        amount: Int
    }

    /// Charge the customer.
    // policy-allow: rule-a because
    step charge { worker "pay.charge" with { amount: input.amount } }

    // Plain.

    step after_blank { compute 1 }

    on failure {
        /// Tell someone.
        step tell { compute 2 }
    }
}
"#;

    #[test]
    fn collects_doc_comments() {
        let c = compile_str("pay.orbit", SRC);
        assert!(!c.has_errors(), "{}", c.render_diagnostics(false));
        let idx = SourceIndex::new(&c);
        let t = &idx.tasks["pay.charge"];
        assert_eq!(t.name.doc.as_deref(), Some("Charges a card.\nSecond line."));
        assert_eq!(
            t.fields["input.amount"].doc.as_deref(),
            Some("Amount in cents.")
        );
        assert_eq!(t.fields["output.id"].doc, None);
        let w = &idx.workflows["Pay"];
        assert_eq!(w.name.doc.as_deref(), Some("Takes payments."));
        assert_eq!(
            w.fields["input.order_id"].doc.as_deref(),
            Some("The order.")
        );
        assert_eq!(w.fields["input.amount"].doc, None);
        // A plain comment between the doc comment and the step ends the block.
        let charge = idx.step("Pay", "", "charge").unwrap();
        assert_eq!(charge.doc, None);
        let comments = &w.step_comments[&(String::new(), "charge".to_string())];
        assert_eq!(comments.len(), 2);
        assert_eq!(comments[1].0.trim(), "policy-allow: rule-a because");
        assert_eq!(idx.step("Pay", "", "after_blank").unwrap().doc, None);
        assert_eq!(
            idx.step("Pay", "on failure", "tell")
                .unwrap()
                .doc
                .as_deref(),
            Some("Tell someone.")
        );
    }

    #[test]
    fn doc_comments_survive_fmt_and_do_not_change_the_hash() {
        let formatted = orbit_syntax::format::format_source(SRC).unwrap();
        assert!(formatted.contains("    /// Charge the customer.\n"));
        assert!(formatted.contains("        /// Amount in cents.\n"));
        let stripped: String = SRC
            .lines()
            .filter(|l| !l.trim_start().starts_with("///"))
            .map(|l| format!("{l}\n"))
            .collect();
        let a = compile_str("a.orbit", SRC);
        let b = compile_str("b.orbit", &stripped);
        assert_eq!(
            a.workflows[0].definition_hash,
            b.workflows[0].definition_hash
        );
    }
}
