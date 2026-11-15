//! `orbit lsp`: a Language Server Protocol server over stdio.
//!
//! Capabilities: live diagnostics (the real compiler, with unsaved buffers
//! overriding files on disk, imports included), hover, go to definition,
//! completion, document symbols and formatting.

use orbit_compiler::{compile_with, Compilation, Loader};
use orbit_ir::{Action, Type, WorkflowIr};
use orbit_syntax::ast::Declaration;
use orbit_syntax::lexer::{tokenize, TokenKind};
use orbit_syntax::Span;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

// ------------------------------------------------------------- transport

fn read_message(input: &mut impl BufRead) -> std::io::Result<Option<Value>> {
    let mut length = None;
    loop {
        let mut line = String::new();
        if input.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            length = v.trim().parse::<usize>().ok();
        }
    }
    let Some(n) = length else { return Ok(None) };
    let mut buf = vec![0; n];
    input.read_exact(&mut buf)?;
    Ok(serde_json::from_slice(&buf).ok())
}

fn send(out: &mut impl Write, msg: &Value) -> std::io::Result<()> {
    let body = msg.to_string();
    write!(out, "Content-Length: {}\r\n\r\n{body}", body.len())?;
    out.flush()
}

// -------------------------------------------------------------- positions

/// LSP position (0-based line, UTF-16 column) of a byte offset.
fn position(text: &str, offset: usize) -> Value {
    let offset = offset.min(text.len());
    let before = &text[..offset];
    let line = before.matches('\n').count();
    let line_start = before.rfind('\n').map(|i| i + 1).unwrap_or(0);
    let character: usize = text[line_start..offset].chars().map(char::len_utf16).sum();
    json!({ "line": line, "character": character })
}

fn range(text: &str, start: usize, end: usize) -> Value {
    json!({ "start": position(text, start), "end": position(text, end) })
}

/// Byte offset of an LSP position.
fn offset(text: &str, pos: &Value) -> usize {
    let line = pos["line"].as_u64().unwrap_or(0) as usize;
    let character = pos["character"].as_u64().unwrap_or(0) as usize;
    let mut start = 0;
    for _ in 0..line {
        match text[start..].find('\n') {
            Some(i) => start += i + 1,
            None => return text.len(),
        }
    }
    let mut units = 0;
    for (i, c) in text[start..].char_indices() {
        if units >= character || c == '\n' {
            return start + i;
        }
        units += c.len_utf16();
    }
    text.len()
}

fn uri_to_path(uri: &str) -> PathBuf {
    let raw = uri.strip_prefix("file://").unwrap_or(uri);
    let mut out = Vec::new();
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&raw[i + 1..i + 3], 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    PathBuf::from(String::from_utf8_lossy(&out).to_string())
}

fn path_to_uri(path: &Path) -> String {
    let s = path.to_string_lossy();
    let encoded: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "/-_.~".contains(c) {
                c.to_string()
            } else {
                format!("%{:02X}", c as u32)
            }
        })
        .collect();
    format!("file://{encoded}")
}

// ---------------------------------------------------------------- documents

/// Open buffers override the files on disk.
struct Overlay<'a>(&'a HashMap<PathBuf, String>);

impl Loader for Overlay<'_> {
    fn load(&self, path: &Path) -> std::io::Result<String> {
        match self.0.get(&canonical(path)) {
            Some(text) => Ok(text.clone()),
            None => std::fs::read_to_string(path),
        }
    }
    fn canonical(&self, path: &Path) -> PathBuf {
        canonical(path)
    }
}

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Declarations found in a compilation's sources, for navigation.
#[derive(Debug, Clone)]
struct Symbol {
    name: String,
    kind: &'static str,
    file: PathBuf,
    /// The name's span (byte offsets within `file`).
    start: usize,
    end: usize,
    /// The whole declaration.
    full: (usize, usize),
    workflow: Option<String>,
}

struct Analysis {
    comp: Compilation,
    symbols: Vec<Symbol>,
}

fn analyze(path: &Path, docs: &HashMap<PathBuf, String>) -> Analysis {
    let comp = compile_with(&[path.to_path_buf()], &Overlay(docs));
    let mut symbols = vec![];
    for (id, file) in comp.sources.files() {
        if file.name == "<prelude>" {
            continue;
        }
        let program = orbit_syntax::parse(id, &file.text).program;
        let fpath = canonical(Path::new(&file.name));
        let mut push =
            |name: &str, kind: &'static str, span: Span, full: Span, workflow: Option<String>| {
                symbols.push(Symbol {
                    name: name.to_string(),
                    kind,
                    file: fpath.clone(),
                    start: span.start as usize,
                    end: span.end as usize,
                    full: (full.start as usize, full.end as usize),
                    workflow,
                })
            };
        for d in &program.declarations {
            match d {
                Declaration::Workflow(w) => {
                    push(&w.name.name, "workflow", w.name.span, w.span, None);
                    for s in &w.steps {
                        push(
                            &s.name.name,
                            "step",
                            s.name.span,
                            s.span,
                            Some(w.name.name.clone()),
                        );
                    }
                }
                Declaration::Schema(s) => push(&s.name.name, "schema", s.name.span, s.span, None),
                Declaration::Task(t) => push(&t.name, "task", t.name_span, t.span, None),
                Declaration::Import(_) => {}
                Declaration::Test(t) => push(&t.name, "test", t.name_span, t.span, None),
            }
        }
    }
    Analysis { comp, symbols }
}

fn diagnostics(a: &Analysis, path: &Path) -> Vec<Value> {
    let target = canonical(path);
    a.comp
        .diagnostics
        .iter()
        .filter(|d| canonical(Path::new(&a.comp.sources.file(d.span.file).name)) == target)
        .map(|d| {
            let text = &a.comp.sources.file(d.span.file).text;
            let mut message = d.message.clone();
            for n in &d.notes {
                message.push_str(&format!("\nnote: {n}"));
            }
            if let Some(h) = &d.help {
                message.push_str(&format!("\nhelp: {h}"));
            }
            json!({
                "range": range(text, d.span.start as usize, d.span.end.max(d.span.start + 1) as usize),
                "severity": if d.is_error() { 1 } else { 2 },
                "code": d.code,
                "source": "orbit",
                "message": message,
            })
        })
        .collect()
}

// ---------------------------------------------------------- word at cursor

struct Word {
    text: String,
    start: usize,
    end: usize,
    is_string: bool,
    /// Path segments up to and including this word, e.g. `["process_payment", "transaction_id"]`.
    path: Vec<String>,
    /// The identifier before a preceding `worker`/`workflow` keyword, if any.
    keyword_before: Option<String>,
}

fn word_at(text: &str, at: usize) -> Option<Word> {
    let (tokens, _) = tokenize(0, text);
    let idx = tokens.iter().position(|t| {
        (t.span.start as usize) <= at && at <= t.span.end as usize && t.kind != TokenKind::Eof
    })?;
    let t = &tokens[idx];
    let (word, is_string) = match &t.kind {
        TokenKind::Ident(w) => (w.clone(), false),
        TokenKind::Str(s) => (s.clone(), true),
        _ => return None,
    };
    let mut path = vec![word.clone()];
    let mut i = idx;
    while i >= 2 && tokens[i - 1].kind == TokenKind::Dot {
        if let TokenKind::Ident(w) = &tokens[i - 2].kind {
            path.insert(0, w.clone());
            i -= 2;
        } else {
            break;
        }
    }
    let keyword_before = match idx.checked_sub(1).map(|j| &tokens[j].kind) {
        Some(TokenKind::Ident(k)) => Some(k.clone()),
        _ => None,
    };
    Some(Word {
        text: word,
        start: t.span.start as usize,
        end: t.span.end as usize,
        is_string,
        path,
        keyword_before,
    })
}

/// The workflow whose source span contains `at` in `path`.
fn enclosing_workflow<'a>(a: &'a Analysis, path: &Path, at: usize) -> Option<&'a WorkflowIr> {
    let target = canonical(path);
    let sym = a
        .symbols
        .iter()
        .find(|s| s.kind == "workflow" && s.file == target && s.full.0 <= at && at <= s.full.1)?;
    a.comp.workflows.iter().find(|w| w.name == sym.name)
}

fn walk(mut ty: Type, fields: &[String]) -> Option<Type> {
    for f in fields {
        let opt = ty.is_optional();
        let next = match ty.unwrap_optional() {
            Type::Record { fields, .. } => fields.iter().find(|x| &x.name == f)?.ty.clone(),
            Type::Json => Type::Json,
            Type::Map { value, .. } => value.as_ref().clone().optional(),
            _ => return None,
        };
        ty = if opt { next.optional() } else { next };
    }
    Some(ty)
}

fn fields_of(ty: &Type) -> Vec<(String, String)> {
    match ty.unwrap_optional() {
        Type::Record { fields, .. } => fields
            .iter()
            .map(|f| (f.name.clone(), f.ty.to_string()))
            .collect(),
        _ => vec![],
    }
}

const KEYWORD_DOCS: &[(&str, &str)] = &[
    ("workflow", "Declares a workflow: a versioned, durable graph of steps."),
    ("step", "A unit of work: a worker task, a child `workflow`, a durable `sleep`, an `await` or a `compute`."),
    ("signal", "`signal <name> { fields }` declares a typed message the workflow accepts."),
    ("await", "`await <signal>` waits durably for the next signal of that name; its payload is the step output."),
    ("query", "`query <name> = <expr>` declares a read-only view of the execution state (steps that have not run read as `null`)."),
    ("compute", "`compute <expr>` evaluates a deterministic expression in the runtime, with no worker."),
    ("on", "`on failure|cancel|timeout { step ... }` runs steps after compensation when the execution fails, is cancelled or times out; they can read `failure`. `on event <signal> { step ... }` runs steps every time that signal arrives; they can read `event`."),
    ("event", "Inside `on event <signal>` blocks: the signal payload."),
    ("limits", "`limits { max_fan_out, max_history_events, max_payload_bytes }` tightens the server's resource limits for this workflow."),
    ("continue_as_new", "`continue_as_new [when <cond>] with <input>`: when every step is done and the condition holds, the run ends and a new run of the same execution starts with this input and an empty history (instead of `return`)."),
    ("failure", "Inside `on` handlers: `failure.reason` (failed, cancelled, timed_out), `failure.code`, `failure.message`."),
    ("compensated_by", "On a task contract: steps calling this task must compensate with the named task (W0601/W0602)."),
    ("worker", "`worker \"task.name\"` runs a task implemented by a worker in any language."),
    ("task", "Declares a worker contract: version, input and output types."),
    ("schema", "A named record type."),
    ("with", "The input passed to the task, type-checked against its contract."),
    ("when", "Runs the step only if the condition is true; otherwise it is skipped and its output is `null`."),
    ("after", "Explicit ordering. Data references add dependencies automatically."),
    ("retry", "`retry { attempts, backoff: fixed | exponential, initial_delay, max_delay, non_retryable }`"),
    ("timeout", "Per-attempt limit (in a step) or overall deadline (in a workflow)."),
    ("queue_timeout", "Fails the step if no worker picks it up in time."),
    ("idempotency_key", "Stable across retries and redeliveries; defaults to `<execution>/<step>`."),
    ("compensate", "The business action that undoes this step if the workflow fails, is cancelled or times out."),
    ("sleep", "A durable timer kept by the runtime; survives restarts."),
    ("for", "`for item in <list>` runs the step once per element (`step[i]`); the output is the list of results."),
    ("max_parallel", "Limits how many elements of a `for` step run at once."),
    ("using", "Routes the task only to workers that announce this runtime (e.g. `python`)."),
    ("input", "The workflow input (or a task's input type)."),
    ("output", "The workflow's declared result type."),
    ("return", "Maps step results to the workflow output."),
    ("execution", "`execution.id`, `execution.workflow`, `execution.version`."),
    ("import", "`import \"other.orbit\"` brings in declarations from another file."),
];

const BUILTIN_DOCS: &[(&str, &str)] = &[
    ("len", "len(String | List | Map) -> Int"),
    ("coalesce", "coalesce(a, b, ...) -> first non-null argument"),
    ("is_null", "is_null(x) -> Bool"),
    ("string", "string(x) -> String"),
    ("int", "int(Int | Float | String | Bool) -> Int"),
    ("float", "float(Int | Float | String | Bool) -> Float"),
    ("sum", "sum(List<Int | Float>) -> Int | Float"),
    ("map", "map(List<T>, x => U) -> List<U>"),
    ("filter", "filter(List<T>, x => Bool) -> List<T>"),
    ("find", "find(List<T>, x => Bool) -> T?"),
    ("any", "any(List<T>, x => Bool) -> Bool"),
    ("all", "all(List<T>, x => Bool) -> Bool"),
    (
        "contains",
        "contains(String, String) | contains(List<T>, T) | contains(Map, String) -> Bool",
    ),
    ("lower", "lower(String) -> String"),
    ("upper", "upper(String) -> String"),
    ("trim", "trim(String) -> String"),
    ("join", "join(List<String>, String) -> String"),
    ("keys", "keys(Map<String, T>) -> List<String>"),
    ("min", "min(number, ...) -> number"),
    ("max", "max(number, ...) -> number"),
    ("abs", "abs(number) -> number"),
];

fn hover(a: &Analysis, path: &Path, text: &str, at: usize) -> Option<Value> {
    let w = word_at(text, at)?;
    let md = |s: String| {
        Some(
            json!({ "contents": { "kind": "markdown", "value": s }, "range": range(text, w.start, w.end) }),
        )
    };
    if w.is_string {
        if w.keyword_before.as_deref() == Some("workflow") {
            let key = format!("workflow:{}", w.text);
            let c = a
                .comp
                .workflows
                .iter()
                .flat_map(|wf| wf.contracts.get(&key))
                .next()?;
            return md(format!(
                "**child workflow** `{}@{}`\n\n- input: `{}`\n- output: `{}`",
                w.text, c.version, c.input, c.output
            ));
        }
        let c = a.comp.contracts.get(&w.text)?;
        let kind = if c.builtin { "built-in task" } else { "task" };
        return md(format!(
            "**{kind}** `{}@{}`\n\n- input: `{}`\n- output: `{}`",
            c.name, c.version, c.input, c.output
        ));
    }
    let wf = enclosing_workflow(a, path, at);
    if let Some(wf) = wf {
        // `input.a.b`, `<step>.a.b`
        if w.path.len() > 1 || w.path[0] == "input" {
            let root = &w.path[0];
            let base = if root == "input" {
                Some(wf.input.clone())
            } else {
                wf.step(root).map(|s| s.output_type.clone())
            };
            if let Some(ty) = base.and_then(|t| walk(t, &w.path[1..])) {
                return md(format!("`{}`: `{ty}`", w.path.join(".")));
            }
        }
        if let Some(step) = wf.step(&w.text) {
            let what = match &step.action {
                Action::Task(i) => format!("worker `{}@{}`", i.task, i.contract_version),
                Action::Workflow(i) => format!(
                    "child workflow `{}`",
                    i.task.trim_start_matches("workflow:")
                ),
                Action::Sleep { duration_ms } => format!("sleep {duration_ms} ms"),
                Action::Await { signal, .. } => format!("waits for signal `{signal}`"),
                Action::Compute { .. } => "computed in the runtime".to_string(),
            };
            let mut s = format!(
                "**step** `{}` — {what}\n\noutput: `{}`",
                step.id, step.output_type
            );
            if !step.dependencies.is_empty() {
                s.push_str(&format!("\n\nafter: {}", step.dependencies.join(", ")));
            }
            if let Some(f) = &step.for_each {
                s.push_str(&format!("\n\nfor each `{}`", f.variable));
            }
            return md(s);
        }
    }
    if let Some(sym) = a
        .symbols
        .iter()
        .find(|s| s.kind == "schema" && s.name == w.text)
    {
        let src = std::fs::read_to_string(&sym.file).unwrap_or_default();
        let body = src.get(sym.full.0..sym.full.1).unwrap_or("").to_string();
        return md(format!("```orbit\n{body}\n```"));
    }
    if let Some((_, sig)) = BUILTIN_DOCS.iter().find(|(n, _)| *n == w.text) {
        return md(format!("**built-in** `{sig}`"));
    }
    if let Some((_, doc)) = KEYWORD_DOCS.iter().find(|(n, _)| *n == w.text) {
        return md(format!("**{}** — {doc}", w.text));
    }
    None
}

fn definition(a: &Analysis, path: &Path, text: &str, at: usize) -> Option<Value> {
    let w = word_at(text, at)?;
    let target = canonical(path);
    let pick = |kind: &str, name: &str| -> Option<&Symbol> {
        let mut c = a
            .symbols
            .iter()
            .filter(|s| s.kind == kind && s.name == name);
        if kind == "step" {
            let wf = a
                .symbols
                .iter()
                .find(|s| {
                    s.kind == "workflow" && s.file == target && s.full.0 <= at && at <= s.full.1
                })
                .map(|s| s.name.clone());
            return c.find(|s| s.workflow == wf);
        }
        c.next()
    };
    let sym = if w.is_string {
        if w.keyword_before.as_deref() == Some("workflow") {
            pick("workflow", &w.text)
        } else {
            pick("task", &w.text)
        }
    } else {
        pick("step", &w.path[0])
            .or_else(|| pick("schema", &w.text))
            .or_else(|| pick("workflow", &w.text))
    }?;
    let src = std::fs::read_to_string(&sym.file).unwrap_or_default();
    let src = if sym.file == target {
        text.to_string()
    } else {
        src
    };
    Some(json!({ "uri": path_to_uri(&sym.file), "range": range(&src, sym.start, sym.end) }))
}

fn completion(a: &Analysis, path: &Path, text: &str, at: usize) -> Value {
    let line_start = text[..at].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let before = &text[line_start..at];
    let item = |label: &str, kind: u32, detail: &str| json!({ "label": label, "kind": kind, "detail": detail });
    let mut items = vec![];
    // After `something.` (possibly with a partial word): fields.
    let trimmed = before.trim_end_matches(|c: char| c.is_ascii_alphanumeric() || c == '_');
    if let Some(prefix) = trimmed.strip_suffix('.') {
        let chain: Vec<String> = prefix
            .rsplit(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '.'))
            .next()
            .unwrap_or("")
            .split('.')
            .map(str::to_string)
            .collect();
        if let (Some(wf), Some(root)) = (enclosing_workflow(a, path, at), chain.first()) {
            let base = if root == "input" {
                Some(wf.input.clone())
            } else if root == "execution" {
                return json!(["id", "workflow", "version"]
                    .iter()
                    .map(|f| item(f, 5, "String"))
                    .collect::<Vec<_>>());
            } else {
                wf.step(root).map(|s| s.output_type.clone())
            };
            if let Some(ty) = base.and_then(|t| walk(t, &chain[1..])) {
                return json!(fields_of(&ty)
                    .iter()
                    .map(|(n, t)| item(n, 5, t))
                    .collect::<Vec<_>>());
            }
        }
        return json!([]);
    }
    // Inside `worker "`: task names.
    if before
        .trim_end_matches(|c: char| c != '"')
        .trim_end_matches('"')
        .trim_end()
        .ends_with("worker")
        && before.matches('"').count() % 2 == 1
    {
        for c in a.comp.contracts.values() {
            items.push(item(&c.name, 3, &format!("{} -> {}", c.input, c.output)));
        }
        return json!(items);
    }
    if let Some(wf) = enclosing_workflow(a, path, at) {
        for s in &wf.steps {
            items.push(item(&s.id, 6, &s.output_type.to_string()));
        }
        items.push(item("input", 6, &wf.input.to_string()));
        items.push(item("execution", 6, "Execution"));
    }
    for s in a.symbols.iter().filter(|s| s.kind == "schema") {
        items.push(item(&s.name, 22, "schema"));
    }
    for t in [
        "String",
        "Int",
        "Float",
        "Bool",
        "Bytes",
        "Duration",
        "Timestamp",
        "UUID",
        "JSON",
        "List",
        "Map",
    ] {
        items.push(item(t, 25, "type"));
    }
    for (n, sig) in BUILTIN_DOCS {
        items.push(item(n, 3, sig));
    }
    for (k, doc) in KEYWORD_DOCS {
        items.push(item(k, 14, doc));
    }
    json!(items)
}

fn symbols(a: &Analysis, path: &Path, text: &str) -> Value {
    let target = canonical(path);
    let mine: Vec<&Symbol> = a.symbols.iter().filter(|s| s.file == target).collect();
    let sym = |s: &Symbol, kind: u32, children: Vec<Value>| {
        json!({
            "name": s.name, "kind": kind,
            "range": range(text, s.full.0, s.full.1),
            "selectionRange": range(text, s.start, s.end),
            "children": children,
        })
    };
    let mut out = vec![];
    for s in &mine {
        match s.kind {
            "workflow" => {
                let steps = mine
                    .iter()
                    .filter(|c| c.kind == "step" && c.workflow.as_deref() == Some(s.name.as_str()))
                    .map(|c| sym(c, 12, vec![]))
                    .collect();
                out.push(sym(s, 5, steps));
            }
            "schema" => out.push(sym(s, 23, vec![])),
            "task" => out.push(sym(s, 11, vec![])),
            "test" => out.push(sym(s, 6, vec![])),
            _ => {}
        }
    }
    json!(out)
}

// ------------------------------------------------------------------ server

pub fn run() -> Result<(), String> {
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let mut docs: HashMap<PathBuf, String> = HashMap::new();
    let mut shutting_down = false;

    let publish = |out: &mut std::io::StdoutLock,
                   docs: &HashMap<PathBuf, String>,
                   uri: &str|
     -> std::io::Result<()> {
        let path = uri_to_path(uri);
        let a = analyze(&path, docs);
        send(
            out,
            &json!({
                "jsonrpc": "2.0",
                "method": "textDocument/publishDiagnostics",
                "params": { "uri": uri, "diagnostics": diagnostics(&a, &path) },
            }),
        )
    };

    loop {
        let Some(msg) = read_message(&mut input).map_err(|e| e.to_string())? else {
            return Ok(());
        };
        let method = msg["method"].as_str().unwrap_or("");
        let id = msg.get("id").cloned();
        let params = &msg["params"];
        let uri = params["textDocument"]["uri"]
            .as_str()
            .unwrap_or("")
            .to_string();
        let doc_text = || {
            docs.get(&canonical(&uri_to_path(&uri)))
                .cloned()
                .unwrap_or_else(|| std::fs::read_to_string(uri_to_path(&uri)).unwrap_or_default())
        };
        let result: Option<Value> = match method {
            "initialize" => Some(json!({
                "capabilities": {
                    "textDocumentSync": 1,
                    "hoverProvider": true,
                    "definitionProvider": true,
                    "documentFormattingProvider": true,
                    "documentSymbolProvider": true,
                    "completionProvider": { "triggerCharacters": [".", "\""] },
                },
                "serverInfo": { "name": "orbit-lsp", "version": env!("CARGO_PKG_VERSION") },
            })),
            "initialized" => None,
            "shutdown" => {
                shutting_down = true;
                Some(Value::Null)
            }
            "exit" => {
                return if shutting_down {
                    Ok(())
                } else {
                    Err("exit before shutdown".into())
                }
            }
            "textDocument/didOpen" => {
                let text = params["textDocument"]["text"]
                    .as_str()
                    .unwrap_or("")
                    .to_string();
                docs.insert(canonical(&uri_to_path(&uri)), text);
                publish(&mut out, &docs, &uri).map_err(|e| e.to_string())?;
                None
            }
            "textDocument/didChange" => {
                if let Some(text) = params["contentChanges"]
                    .as_array()
                    .and_then(|c| c.last())
                    .and_then(|c| c["text"].as_str())
                {
                    docs.insert(canonical(&uri_to_path(&uri)), text.to_string());
                }
                publish(&mut out, &docs, &uri).map_err(|e| e.to_string())?;
                None
            }
            "textDocument/didSave" => {
                publish(&mut out, &docs, &uri).map_err(|e| e.to_string())?;
                None
            }
            "textDocument/didClose" => {
                docs.remove(&canonical(&uri_to_path(&uri)));
                send(&mut out, &json!({ "jsonrpc": "2.0", "method": "textDocument/publishDiagnostics", "params": { "uri": uri, "diagnostics": [] } }))
                    .map_err(|e| e.to_string())?;
                None
            }
            "textDocument/hover"
            | "textDocument/definition"
            | "textDocument/completion"
            | "textDocument/documentSymbol" => {
                let path = uri_to_path(&uri);
                let text = doc_text();
                let at = offset(&text, &params["position"]);
                let a = analyze(&path, &docs);
                Some(match method {
                    "textDocument/hover" => hover(&a, &path, &text, at).unwrap_or(Value::Null),
                    "textDocument/definition" => {
                        definition(&a, &path, &text, at).unwrap_or(Value::Null)
                    }
                    "textDocument/completion" => completion(&a, &path, &text, at),
                    _ => symbols(&a, &path, &text),
                })
            }
            "textDocument/formatting" => {
                let text = doc_text();
                Some(match orbit_syntax::format::format_source(&text) {
                    Ok(f) if f != text => {
                        json!([{ "range": range(&text, 0, text.len()), "newText": f }])
                    }
                    _ => json!([]),
                })
            }
            _ if id.is_some() => {
                send(&mut out, &json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": format!("method not found: {method}") } }))
                    .map_err(|e| e.to_string())?;
                None
            }
            _ => None,
        };
        if let (Some(id), Some(result)) = (id, result) {
            send(
                &mut out,
                &json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            )
            .map_err(|e| e.to_string())?;
        }
    }
}
