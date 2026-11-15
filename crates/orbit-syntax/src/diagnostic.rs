//! Compiler diagnostics: error codes, labelled spans, help text and rendering.

use crate::source::{Location, SourceMap, Span};
use serde::Serialize;
use std::fmt::Write as _;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    Warning,
}

#[derive(Debug, Clone)]
pub struct Label {
    pub span: Span,
    pub message: String,
}

/// A single compiler message. Every diagnostic carries a stable error code
/// (see `docs/src/reference/diagnostics.md`), a primary source location and optional help.
#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub severity: Severity,
    pub code: &'static str,
    pub message: String,
    pub span: Span,
    pub labels: Vec<Label>,
    pub notes: Vec<String>,
    pub help: Option<String>,
}

impl Diagnostic {
    pub fn error(code: &'static str, message: impl Into<String>, span: Span) -> Self {
        Diagnostic {
            severity: Severity::Error,
            code,
            message: message.into(),
            span,
            labels: vec![],
            notes: vec![],
            help: None,
        }
    }

    pub fn warning(code: &'static str, message: impl Into<String>, span: Span) -> Self {
        Diagnostic {
            severity: Severity::Warning,
            ..Diagnostic::error(code, message, span)
        }
    }

    pub fn with_label(mut self, span: Span, message: impl Into<String>) -> Self {
        self.labels.push(Label {
            span,
            message: message.into(),
        });
        self
    }

    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        self.notes.push(note.into());
        self
    }

    pub fn with_help(mut self, help: impl Into<String>) -> Self {
        self.help = Some(help.into());
        self
    }

    pub fn is_error(&self) -> bool {
        self.severity == Severity::Error
    }

    /// Render in a rustc-like format with a source snippet and carets.
    pub fn render(&self, sources: &SourceMap, color: bool) -> String {
        let (red, yellow, blue, bold, reset) = if color {
            ("\x1b[31m", "\x1b[33m", "\x1b[34m", "\x1b[1m", "\x1b[0m")
        } else {
            ("", "", "", "", "")
        };
        let sev_color = if self.is_error() { red } else { yellow };
        let sev = match self.severity {
            Severity::Error => "error",
            Severity::Warning => "warning",
        };
        let mut out = String::new();
        let _ = writeln!(
            out,
            "{bold}{sev_color}{sev}[{}]{reset}{bold}: {}{reset}",
            self.code, self.message
        );
        let loc = sources.location(self.span);
        let file = sources.file(self.span.file);
        let gutter = loc.line.to_string().len().max(
            self.labels
                .iter()
                .map(|l| sources.location(l.span).line.to_string().len())
                .max()
                .unwrap_or(0),
        );
        let pad = " ".repeat(gutter);
        let _ = writeln!(out, "{pad}{blue}-->{reset} {loc}");
        let _ = writeln!(out, "{pad} {blue}|{reset}");
        let mut snippets: Vec<(Span, Option<&str>, bool)> = vec![(self.span, None, true)];
        for l in &self.labels {
            snippets.push((l.span, Some(l.message.as_str()), false));
        }
        for (span, msg, primary) in snippets {
            if span.file != self.span.file {
                let l = sources.location(span);
                let _ = writeln!(
                    out,
                    "{pad} {blue}={reset} note: {} ({l})",
                    msg.unwrap_or("")
                );
                continue;
            }
            let (line, col) = file.line_col(span.start as usize);
            let (end_line, end_col) = file.line_col(span.end as usize);
            let text = file.line_text(line);
            let width = if end_line == line {
                (end_col.saturating_sub(col)).max(1)
            } else {
                text.chars().count().saturating_sub(col - 1).max(1)
            };
            let marker = if primary { "^" } else { "-" };
            let c = if primary { sev_color } else { blue };
            let _ = writeln!(out, "{blue}{:>gutter$} |{reset} {text}", line);
            let _ = writeln!(
                out,
                "{pad} {blue}|{reset} {}{c}{}{}{reset}",
                " ".repeat(col - 1),
                marker.repeat(width),
                msg.map(|m| format!(" {m}")).unwrap_or_default()
            );
        }
        for n in &self.notes {
            let _ = writeln!(out, "{pad} {blue}={reset} {bold}note{reset}: {n}");
        }
        if let Some(h) = &self.help {
            let _ = writeln!(out, "{pad} {blue}={reset} {bold}help{reset}: {h}");
        }
        out
    }

    /// Machine-readable form, used by `orbit check --format json` and editors.
    pub fn to_json(&self, sources: &SourceMap) -> JsonDiagnostic {
        let end = sources
            .file(self.span.file)
            .line_col(self.span.end as usize);
        JsonDiagnostic {
            severity: self.severity,
            code: self.code,
            message: self.message.clone(),
            location: sources.location(self.span),
            end_line: end.0,
            end_column: end.1,
            labels: self
                .labels
                .iter()
                .map(|l| JsonLabel {
                    location: sources.location(l.span),
                    message: l.message.clone(),
                })
                .collect(),
            notes: self.notes.clone(),
            help: self.help.clone(),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct JsonDiagnostic {
    pub severity: Severity,
    pub code: &'static str,
    pub message: String,
    pub location: Location,
    pub end_line: usize,
    pub end_column: usize,
    pub labels: Vec<JsonLabel>,
    pub notes: Vec<String>,
    pub help: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct JsonLabel {
    pub location: Location,
    pub message: String,
}

/// Compute the Levenshtein distance; used for "did you mean" suggestions.
pub fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            let cost = if ca == *cb { 0 } else { 1 };
            cur.push((prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1));
        }
        prev = cur;
    }
    prev[b.len()]
}

/// Best "did you mean" candidate, if any is close enough.
pub fn suggest<'a>(name: &str, candidates: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    let limit = (name.len() / 3).max(1) + 1;
    candidates
        .into_iter()
        .map(|c| (edit_distance(name, c), c))
        .filter(|(d, _)| *d <= limit)
        .min_by_key(|(d, c)| (*d, c.to_string()))
        .map(|(_, c)| c)
}
