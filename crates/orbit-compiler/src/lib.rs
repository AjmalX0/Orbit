//! The ORBIT compiler.
//!
//! Stages (RFC-0001 §12.1): source loading → lexing → parsing → symbol
//! resolution → type checking → dependency graph construction → workflow
//! validation → typed IR generation.

pub mod check;
pub mod compat;
pub mod prelude;
mod testing;
pub mod types;

use check::{Checker, Env};
use orbit_ir::{
    self as ir, Action, Backoff, FieldType, ForEach, Invocation, QueryIr, RetryPolicy,
    SourceLocation, StepIr, TaskContract, Type, WorkflowIr, WORKFLOW_CONTRACT_PREFIX,
};
use orbit_syntax::ast::{self, Declaration, StepAction};
use orbit_syntax::diagnostic::{suggest, Diagnostic};
use orbit_syntax::{parse, FileId, SourceMap, Span};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use types::{assignable, Schemas, KNOWN_LANGUAGES};

pub const COMPILER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Result of compiling one or more source files.
pub struct Compilation {
    pub sources: SourceMap,
    pub diagnostics: Vec<Diagnostic>,
    /// Empty when any error was reported.
    pub workflows: Vec<WorkflowIr>,
    /// Contracts declared by the sources (including the prelude).
    pub contracts: BTreeMap<String, TaskContract>,
    /// Which file each workflow came from.
    pub workflow_files: Vec<FileId>,
    /// `test` declarations, checked against `workflows` (empty on errors).
    /// They are run by `orbit test` and never deployed or compiled to IR files.
    pub tests: Vec<orbit_ir::testing::TestCase>,
}

impl Compilation {
    pub fn has_errors(&self) -> bool {
        self.diagnostics.iter().any(|d| d.is_error())
    }

    pub fn render_diagnostics(&self, color: bool) -> String {
        self.diagnostics
            .iter()
            .map(|d| d.render(&self.sources, color))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Supplies source text for a path; lets tests compile in-memory projects.
pub trait Loader {
    fn load(&self, path: &Path) -> std::io::Result<String>;
    fn canonical(&self, path: &Path) -> PathBuf {
        path.to_path_buf()
    }
}

pub struct FsLoader;

impl Loader for FsLoader {
    fn load(&self, path: &Path) -> std::io::Result<String> {
        std::fs::read_to_string(path)
    }
    fn canonical(&self, path: &Path) -> PathBuf {
        std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
    }
}

/// In-memory loader for tests and embedding.
pub struct MemLoader(pub HashMap<PathBuf, String>);

impl Loader for MemLoader {
    fn load(&self, path: &Path) -> std::io::Result<String> {
        self.0
            .get(path)
            .cloned()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "not found"))
    }
}

pub fn compile_files(paths: &[PathBuf]) -> Compilation {
    compile_with(paths, &FsLoader)
}

/// Compile a single in-memory source (imports are not resolvable).
pub fn compile_str(name: &str, src: &str) -> Compilation {
    let mut files = HashMap::new();
    files.insert(PathBuf::from(name), src.to_string());
    compile_with(&[PathBuf::from(name)], &MemLoader(files))
}

pub fn compile_with(paths: &[PathBuf], loader: &dyn Loader) -> Compilation {
    let mut sources = SourceMap::new();
    let prelude_id = sources.add("<prelude>", prelude::SOURCE);
    let mut diagnostics = vec![];
    let mut programs: Vec<(FileId, ast::Program)> = vec![];
    let mut loaded: HashMap<PathBuf, FileId> = HashMap::new();
    let mut queue: Vec<(PathBuf, Option<Span>)> =
        paths.iter().rev().map(|p| (p.clone(), None)).collect();

    while let Some((path, from)) = queue.pop() {
        let canon = loader.canonical(&path);
        if loaded.contains_key(&canon) {
            continue;
        }
        let text = match loader.load(&path) {
            Ok(t) => t,
            Err(e) => {
                match from {
                    Some(span) => diagnostics.push(Diagnostic::error(
                        "E0206",
                        format!("cannot import `{}`: {e}", path.display()),
                        span,
                    )),
                    None => {
                        let id = sources.add(path.display().to_string(), "");
                        diagnostics.push(Diagnostic::error(
                            "E0206",
                            format!("cannot read `{}`: {e}", path.display()),
                            Span::new(id, 0, 0),
                        ));
                    }
                }
                continue;
            }
        };
        let id = sources.add(path.display().to_string(), text);
        loaded.insert(canon, id);
        let result = parse(id, &sources.file(id).text);
        diagnostics.extend(result.diagnostics);
        let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
        for d in &result.program.declarations {
            if let Declaration::Import(imp) = d {
                queue.push((dir.join(&imp.path), Some(imp.span)));
            }
        }
        programs.push((id, result.program));
    }

    let mut comp = Compilation {
        sources,
        diagnostics,
        workflows: vec![],
        contracts: BTreeMap::new(),
        workflow_files: vec![],
        tests: vec![],
    };
    analyze(&mut comp, prelude_id, &programs);
    if comp.has_errors() {
        comp.workflows.clear();
        comp.tests.clear();
    }
    comp.diagnostics
        .sort_by_key(|d| (d.span.file, d.span.start, d.severity));
    comp
}

// ------------------------------------------------------------------ analysis

fn analyze(comp: &mut Compilation, prelude_id: FileId, programs: &[(FileId, ast::Program)]) {
    let diags = &mut comp.diagnostics;
    let prelude = prelude::program(prelude_id);

    // Symbol collection.
    let mut schema_decls: BTreeMap<String, &ast::Schema> = BTreeMap::new();
    let mut task_decls: BTreeMap<String, &ast::TaskDecl> = BTreeMap::new();
    let mut workflows: Vec<(FileId, &ast::Workflow)> = vec![];
    let all = std::iter::once((prelude_id, &prelude)).chain(programs.iter().map(|(f, p)| (*f, p)));
    for (file, program) in all {
        for decl in &program.declarations {
            match decl {
                Declaration::Schema(s) => {
                    if types::primitive(&s.name.name).is_some()
                        || s.name.name == "List"
                        || s.name.name == "Map"
                    {
                        diags.push(Diagnostic::error(
                            "E0200",
                            format!(
                                "`{}` is a built-in type and cannot be redefined",
                                s.name.name
                            ),
                            s.name.span,
                        ));
                    } else if let Some(prev) = schema_decls.get(&s.name.name) {
                        diags.push(
                            Diagnostic::error(
                                "E0200",
                                format!("schema `{}` is defined more than once", s.name.name),
                                s.name.span,
                            )
                            .with_label(prev.name.span, "first defined here"),
                        );
                    } else {
                        schema_decls.insert(s.name.name.clone(), s);
                    }
                }
                Declaration::Task(t) => {
                    if let Some(prev) = task_decls.get(&t.name) {
                        let mut d = Diagnostic::error(
                            "E0200",
                            format!("task `{}` is declared more than once", t.name),
                            t.name_span,
                        );
                        if prev.span.file != prelude_id {
                            d = d.with_label(prev.name_span, "first declared here");
                        } else {
                            d = d.with_note("this name is reserved by the ORBIT prelude");
                        }
                        diags.push(d);
                    } else {
                        if t.name.starts_with("orbit.") && file != prelude_id {
                            diags.push(Diagnostic::error(
                                "E0200",
                                "task names starting with `orbit.` are reserved for built-in tasks",
                                t.name_span,
                            ));
                        }
                        task_decls.insert(t.name.clone(), t);
                    }
                }
                Declaration::Workflow(w) => {
                    if let Some((_, prev)) =
                        workflows.iter().find(|(_, p)| p.name.name == w.name.name)
                    {
                        diags.push(
                            Diagnostic::error(
                                "E0200",
                                format!("workflow `{}` is defined more than once", w.name.name),
                                w.name.span,
                            )
                            .with_label(prev.name.span, "first defined here"),
                        );
                    } else {
                        workflows.push((file, w));
                    }
                }
                Declaration::Import(_) | Declaration::Test(_) => {}
            }
        }
    }

    let schemas = Schemas {
        decls: schema_decls,
    };
    // Resolve every schema once so that errors in unused schemas are still reported.
    for s in schemas.decls.values() {
        if s.span.file != prelude_id {
            schemas.resolve(&ast::TypeExpr::Named(s.name.clone()), diags);
        }
    }

    // Worker contracts.
    let mut contracts = BTreeMap::new();
    for (name, t) in &task_decls {
        let mut contract_ty = |ty: &Option<ast::TypeExpr>, what: &str| -> Type {
            match ty {
                Some(t) => {
                    let resolved = schemas.resolve(t, diags);
                    if !matches!(resolved, Type::Record { .. } | Type::Json) {
                        diags.push(Diagnostic::error(
                            "E0310",
                            format!("task {what} must be a record type, found `{resolved}`"),
                            t.span(),
                        ));
                    }
                    resolved
                }
                None => Type::record(None, vec![]),
            }
        };
        let input = contract_ty(&t.input, "input");
        let output = contract_ty(&t.output, "output");
        let version = t
            .version
            .as_ref()
            .map(|v| v.0.clone())
            .unwrap_or_else(|| "1.0.0".into());
        if let Some((v, span)) = &t.version {
            if !is_semver(v) {
                diags.push(Diagnostic::warning(
                    "W0201",
                    format!("version `{v}` is not semantic (MAJOR.MINOR.PATCH)"),
                    *span,
                ));
            }
        }
        contracts.insert(
            name.clone(),
            TaskContract {
                name: name.clone(),
                version,
                input,
                output,
                builtin: t.span.file == prelude_id,
                workflow_hash: None,
                compensated_by: t.compensated_by.as_ref().map(|(n, _)| n.clone()),
            },
        );
    }
    // `compensated_by` must name a declared task.
    for (_, program) in programs {
        for d in &program.declarations {
            if let ast::Declaration::Task(t) = d {
                if let Some((by, span)) = &t.compensated_by {
                    if !contracts.contains_key(by) {
                        let mut diag = Diagnostic::error(
                            "E0316",
                            format!("`compensated_by` names unknown task `{by}`"),
                            *span,
                        );
                        if let Some(s) = suggest(by, contracts.keys().map(String::as_str)) {
                            diag = diag.with_help(format!("did you mean `{s}`?"));
                        }
                        diags.push(diag);
                    }
                }
            }
        }
    }

    // Child workflows: lower callees before callers so a caller can pin the
    // callee's definition hash. Cycles between workflows are rejected.
    let names: HashSet<&str> = workflows
        .iter()
        .map(|(_, w)| w.name.name.as_str())
        .collect();
    let mut calls: BTreeMap<&str, BTreeMap<&str, Span>> = BTreeMap::new();
    for (_, wf) in &workflows {
        let entry = calls.entry(wf.name.name.as_str()).or_default();
        for s in &wf.steps {
            if let Some(StepAction::Workflow(child, span)) = &s.action {
                if let Some(n) = names.get(child.as_str()) {
                    entry.entry(*n).or_insert(*span);
                } else {
                    let mut d =
                        Diagnostic::error("E0312", format!("unknown workflow `{child}`"), *span);
                    if let Some(sug) = suggest(child, names.iter().copied()) {
                        d = d.with_help(format!("did you mean `{sug}`?"));
                    } else {
                        d = d.with_help("child workflows must be defined in, or imported into, this compilation");
                    }
                    diags.push(d);
                }
            }
        }
    }
    if let Some(cycle) = find_cycle(&calls) {
        let mut d = Diagnostic::error(
            "E0311",
            "workflows call each other in a cycle",
            calls
                .get(cycle[0])
                .and_then(|m| m.get(cycle[1]))
                .copied()
                .unwrap_or_default(),
        )
        .with_note(format!("cycle: {}", cycle.join(" -> ")))
        .with_help("a workflow cannot (directly or indirectly) start itself as a child");
        for w in cycle.windows(2).skip(1) {
            if let Some(span) = calls.get(w[0]).and_then(|m| m.get(w[1])) {
                d = d.with_label(*span, format!("`{}` starts `{}`", w[0], w[1]));
            }
        }
        diags.push(d);
        comp.contracts = contracts;
        return;
    }
    let mut ordered: Vec<(FileId, &ast::Workflow)> = vec![];
    while ordered.len() < workflows.len() {
        let next = workflows
            .iter()
            .find(|(_, w)| {
                !ordered.iter().any(|(_, o)| o.name.name == w.name.name)
                    && calls[w.name.name.as_str()]
                        .keys()
                        .all(|c| ordered.iter().any(|(_, o)| o.name.name == *c))
            })
            .copied()
            .expect("acyclic");
        ordered.push(next);
    }
    let mut all_contracts = contracts.clone();
    for (file, wf) in ordered {
        let lowered = lower_workflow(wf, &schemas, &all_contracts, &comp.sources, diags);
        // Even when lowering failed, register a placeholder contract so callers
        // do not report a cascade of "unknown workflow" errors.
        let contract = match &lowered {
            Some(ir) => TaskContract {
                name: format!("{WORKFLOW_CONTRACT_PREFIX}{}", ir.name),
                version: ir.version.clone(),
                input: ir.input.clone(),
                output: ir.output.clone(),
                builtin: false,
                workflow_hash: Some(ir.definition_hash.clone()),
                compensated_by: None,
            },
            None => TaskContract {
                name: format!("{WORKFLOW_CONTRACT_PREFIX}{}", wf.name.name),
                version: String::new(),
                input: Type::Json,
                output: Type::Json,
                builtin: false,
                workflow_hash: Some(String::new()),
                compensated_by: None,
            },
        };
        all_contracts.insert(contract.name.clone(), contract);
        if let Some(ir) = lowered {
            comp.workflows.push(ir);
            comp.workflow_files.push(file);
        }
    }
    comp.contracts = contracts;
    comp.tests = testing::lower_tests(programs, &comp.workflows, &names, &comp.sources, diags);
}

pub fn is_semver(v: &str) -> bool {
    let core = v.split(['-', '+']).next().unwrap_or("");
    let parts: Vec<_> = core.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

/// Root names that reference steps in an expression.
fn step_refs<'a>(e: &'a ast::Expr, steps: &HashSet<&str>, out: &mut Vec<&'a ast::Ident>) {
    e.walk_paths(&mut |segs, _| {
        if steps.contains(segs[0].name.as_str()) {
            out.push(&segs[0]);
        }
    });
}

const RESERVED_STEP_NAMES: &[&str] = &["input", "execution", "output", "if", "then", "else"];

fn lower_workflow(
    wf: &ast::Workflow,
    schemas: &Schemas,
    contracts: &BTreeMap<String, TaskContract>,
    sources: &SourceMap,
    diags: &mut Vec<Diagnostic>,
) -> Option<WorkflowIr> {
    let start_errors = diags.iter().filter(|d| d.is_error()).count();
    let version = match &wf.version {
        Some((v, span)) => {
            if !is_semver(v) {
                diags.push(Diagnostic::warning(
                    "W0201",
                    format!("version `{v}` is not semantic (MAJOR.MINOR.PATCH)"),
                    *span,
                ));
            }
            v.clone()
        }
        None => {
            diags.push(
                Diagnostic::error("E0500", format!("workflow `{}` has no version", wf.name.name), wf.name.span)
                    .with_help("add `version \"1.0.0\"`; running executions stay pinned to the version they started with"),
            );
            String::new()
        }
    };

    let input_ty = Type::record(None, schemas.fields(&wf.input, diags, &mut vec![]));
    let declared_output = wf
        .output
        .as_ref()
        .map(|(fields, _)| Type::record(None, schemas.fields(fields, diags, &mut vec![])));

    // --- Signals: typed messages the workflow accepts.
    let mut signal_types: BTreeMap<String, Type> = BTreeMap::new();
    let mut signal_spans: BTreeMap<&str, Span> = BTreeMap::new();
    for sig in &wf.signals {
        if let Some(prev) = signal_spans.get(sig.name.name.as_str()) {
            diags.push(
                Diagnostic::error(
                    "E0317",
                    format!("signal `{}` is declared more than once", sig.name.name),
                    sig.name.span,
                )
                .with_label(*prev, "first declared here"),
            );
            continue;
        }
        signal_spans.insert(&sig.name.name, sig.name.span);
        let ty = Type::record(None, schemas.fields(&sig.fields, diags, &mut vec![]));
        signal_types.insert(sig.name.name.clone(), ty);
    }

    // --- Steps: names, actions and output types.
    let mut by_name: BTreeMap<&str, &ast::Step> = BTreeMap::new();
    for s in &wf.steps {
        if RESERVED_STEP_NAMES.contains(&s.name.name.as_str()) {
            diags.push(Diagnostic::error(
                "E0306",
                format!(
                    "`{}` is reserved and cannot be used as a step name",
                    s.name.name
                ),
                s.name.span,
            ));
            continue;
        }
        if let Some(prev) = by_name.get(s.name.name.as_str()) {
            diags.push(
                Diagnostic::error(
                    "E0302",
                    format!("step `{}` is defined more than once", s.name.name),
                    s.name.span,
                )
                .with_label(prev.name.span, "first defined here"),
            );
            continue;
        }
        by_name.insert(&s.name.name, s);
    }
    let step_names: HashSet<&str> = by_name.keys().copied().collect();
    let resolve_task = |name: &str,
                        span: Span,
                        diags: &mut Vec<Diagnostic>|
     -> Option<&TaskContract> {
        match contracts.get(name) {
            Some(c) => Some(c),
            None => {
                let mut d =
                    Diagnostic::error("E0300", format!("unknown worker task `{name}`"), span);
                if let Some(s) = suggest(name, contracts.keys().map(String::as_str)) {
                    d = d.with_help(format!("did you mean `{s}`?"));
                } else {
                    d = d.with_help(format!(
                        "declare its contract: `task \"{name}\" {{ version \"1.0.0\" input {{ ... }} output {{ ... }} }}`"
                    ));
                }
                diags.push(d);
                None
            }
        }
    };

    // Set when a step names an unknown child workflow. That error (E0312) is
    // reported before lowering, so it must stop IR generation explicitly.
    let mut missing_child = false;
    let mut step_types: BTreeMap<String, Type> = BTreeMap::new();
    let mut step_contracts: HashMap<&str, Option<&TaskContract>> = HashMap::new();
    for (name, s) in &by_name {
        let out = match &s.action {
            Some(StepAction::Worker(task, span)) => {
                let c = resolve_task(task, *span, diags);
                step_contracts.insert(name, c);
                c.map(|c| c.output.clone()).unwrap_or(Type::Json)
            }
            Some(StepAction::Workflow(child, _)) => {
                // Unknown children were already reported (E0312).
                let c = contracts.get(&format!("{WORKFLOW_CONTRACT_PREFIX}{child}"));
                missing_child |= c.is_none();
                step_contracts.insert(name, c);
                c.map(|c| c.output.clone()).unwrap_or(Type::Json)
            }
            Some(StepAction::Sleep(..)) => Type::record(None, vec![]),
            Some(StepAction::Await(sig)) => match signal_types.get(&sig.name) {
                Some(t) => t.clone(),
                None => {
                    let mut d = Diagnostic::error(
                        "E0315",
                        format!("unknown signal `{}`", sig.name),
                        sig.span,
                    );
                    if let Some(sug) = suggest(&sig.name, signal_types.keys().map(String::as_str)) {
                        d = d.with_help(format!("did you mean `{sug}`?"));
                    } else {
                        d = d.with_help(format!(
                            "declare it in the workflow: `signal {} {{ ... }}`",
                            sig.name
                        ));
                    }
                    diags.push(d);
                    Type::Json
                }
            },
            // Typed in dependency order below, once the steps it reads are typed.
            Some(StepAction::Compute(..)) => Type::Json,
            None => {
                diags.push(
                    Diagnostic::error("E0301", format!("step `{name}` does nothing"), s.name.span)
                        .with_help(
                            "add `worker \"task.name\"`, `workflow \"Name\"`, `sleep 10s`, `await <signal>` or `compute <expression>`",
                        ),
                );
                Type::Json
            }
        };
        let out = if s.body.for_each.is_some() {
            Type::List {
                item: Box::new(out),
            }
        } else {
            out
        };
        let out = if s.body.condition.is_some() {
            out.optional()
        } else {
            out
        };
        step_types.insert(name.to_string(), out);
    }

    // --- Dependencies (explicit `after` + implicit data references).
    let mut deps: BTreeMap<&str, BTreeMap<&str, Span>> = BTreeMap::new();
    for (name, s) in &by_name {
        let entry = deps.entry(name).or_default();
        for a in &s.body.after {
            if a.name == **name {
                diags.push(Diagnostic::error(
                    "E0305",
                    format!("step `{name}` cannot run after itself"),
                    a.span,
                ));
            } else if step_names.contains(a.name.as_str()) {
                entry
                    .entry(by_name.get_key_value(a.name.as_str()).unwrap().0)
                    .or_insert(a.span);
            } else {
                let mut d = Diagnostic::error(
                    "E0303",
                    format!("`after` references unknown step `{}`", a.name),
                    a.span,
                );
                if let Some(sug) = suggest(&a.name, step_names.iter().copied()) {
                    d = d.with_help(format!("did you mean `{sug}`?"));
                }
                diags.push(d);
            }
        }
        let mut refs = vec![];
        let items = s.body.for_each.as_ref().map(|f| &f.items);
        let computed = match &s.action {
            Some(StepAction::Compute(e, _)) => Some(e),
            _ => None,
        };
        for e in [
            s.body.with.as_ref(),
            s.body.condition.as_ref(),
            s.body.idempotency_key.as_ref(),
            items,
            computed,
        ]
        .into_iter()
        .flatten()
        {
            step_refs(e, &step_names, &mut refs);
        }
        for r in refs {
            if r.name == **name {
                diags.push(
                    Diagnostic::error("E0305", format!("step `{name}` cannot read its own output"), r.span)
                        .with_help("a step's output can be read by later steps, its `compensate` block, or `return`"),
                );
            } else {
                entry
                    .entry(by_name.get_key_value(r.name.as_str()).unwrap().0)
                    .or_insert(r.span);
            }
        }
    }

    // --- Cycle detection with a readable path.
    if let Some(cycle) = find_cycle(&deps) {
        let first = by_name[cycle[0]];
        let mut d = Diagnostic::error(
            "E0304",
            "circular dependency between steps",
            first.name.span,
        )
        .with_note(format!("cycle: {}", cycle.join(" -> ")));
        for w in cycle.windows(2) {
            if let Some(span) = deps.get(w[0]).and_then(|m| m.get(w[1])) {
                d = d.with_label(*span, format!("`{}` depends on `{}`", w[0], w[1]));
            }
        }
        d = d.with_help("the step graph must be acyclic; remove one of these dependencies");
        diags.push(d);
        return None;
    }

    // --- Deterministic topological order (ties broken by source order).
    let mut source_order: Vec<&str> = vec![];
    for s in &wf.steps {
        let n = s.name.name.as_str();
        if by_name.contains_key(n) && !source_order.contains(&n) {
            source_order.push(n);
        }
    }
    let mut order: Vec<&str> = vec![];
    let mut done: HashSet<&str> = HashSet::new();
    while order.len() < source_order.len() {
        let next = source_order
            .iter()
            .find(|n| !done.contains(*n) && deps[*n].keys().all(|d| done.contains(d)))
            .copied()
            .expect("acyclic");
        done.insert(next);
        order.push(next);
    }
    let ancestors = |id: &str| -> HashSet<String> {
        let mut out = HashSet::new();
        let mut stack: Vec<&str> = deps[id].keys().copied().collect();
        while let Some(n) = stack.pop() {
            if out.insert(n.to_string()) {
                stack.extend(deps[n].keys().copied());
            }
        }
        out
    };

    // --- Type-check and lower each step.
    let mut steps_ir = vec![];
    let mut source_map = BTreeMap::new();
    let loc = |span: Span| {
        let l = sources.location(span);
        SourceLocation {
            file: l.file,
            line: l.line,
            column: l.column,
        }
    };
    source_map.insert("workflow".to_string(), loc(wf.name.span));
    let mut used_contracts: BTreeMap<String, TaskContract> = BTreeMap::new();
    for name in &order {
        let s = by_name[name];
        let mut computed_type: Option<Type> = None;
        source_map.insert(name.to_string(), loc(s.name.span));
        // `for item in <list>`: type the list and bind the loop variable.
        let mut locals: Vec<(String, Type)> = vec![];
        let for_each = s.body.for_each.as_ref().map(|f| {
            let env = Env {
                input: Some(&input_ty),
                steps: &step_types,
                allowed: None,
                context: "a step",
                locals: &[],
            };
            let mut ck = Checker { env: &env, diags };
            let (t, items) = ck.check(&f.items, None);
            let element = match t.unwrap_optional() {
                Type::List { item } => (**item).clone(),
                Type::Json => Type::Json,
                other => {
                    ck.diags.push(
                        Diagnostic::error(
                            "E0414",
                            format!("`for` needs a list, found `{other}`"),
                            f.items.span(),
                        )
                        .with_help(
                            "iterate over a `List<T>` value, e.g. `for item in input.items`",
                        ),
                    );
                    Type::Json
                }
            };
            let var = &f.variable.name;
            if step_names.contains(var.as_str()) || ["input", "execution"].contains(&var.as_str()) {
                let what = if step_names.contains(var.as_str()) {
                    "a step"
                } else {
                    "a built-in name"
                };
                ck.diags.push(
                    Diagnostic::error(
                        "E0413",
                        format!("loop variable `{var}` shadows {what}"),
                        f.variable.span,
                    )
                    .with_help("choose a different name, e.g. `item`"),
                );
            }
            if matches!(s.action, Some(StepAction::Sleep(..))) {
                ck.diags.push(Diagnostic::error(
                    "E0307",
                    "`for` is not allowed in a `sleep` step",
                    f.span,
                ));
            }
            if matches!(s.action, Some(StepAction::Await(..))) {
                ck.diags.push(
                    Diagnostic::error("E0307", "`for` is not allowed in an `await` step", f.span)
                        .with_note("an `await` step receives one signal"),
                );
            }
            locals.push((var.clone(), element));
            let max_parallel = s.body.max_parallel.and_then(|(n, span)| {
                if !(1..=10_000).contains(&n) {
                    ck.diags.push(Diagnostic::error(
                        "E0410",
                        "`max_parallel` must be between 1 and 10000",
                        span,
                    ));
                    None
                } else {
                    Some(n as u32)
                }
            });
            ForEach {
                variable: var.clone(),
                items,
                max_parallel,
            }
        });
        if let (None, Some((_, span))) = (&s.body.for_each, s.body.max_parallel) {
            diags.push(
                Diagnostic::error("E0307", "`max_parallel` only applies to `for` steps", span)
                    .with_help("add `for item in <list>` to fan the step out"),
            );
        }
        let env = Env {
            input: Some(&input_ty),
            steps: &step_types,
            allowed: None,
            context: "a step",
            locals: &locals,
        };
        // `when` is evaluated once for the whole step, before any fan-out, so
        // the loop variable is not in scope there.
        let condition = s.body.condition.as_ref().map(|c| {
            let cond_env = Env { input: Some(&input_ty), steps: &step_types, allowed: None, context: "a step", locals: &[] };
            let mut cck = Checker { env: &cond_env, diags: &mut *diags };
            if let Some(f) = &s.body.for_each {
                let var = &f.variable.name;
                let mut uses = false;
                c.walk_paths(&mut |segs, _| uses |= segs[0].name == *var);
                if uses {
                    cck.diags.push(
                        Diagnostic::error("E0401", format!("loop variable `{var}` is not in scope in `when`"), c.span())
                            .with_note("`when` decides once whether the whole `for` step runs, before it fans out")
                            .with_help("to process only some elements, filter the list in a previous step"),
                    );
                    return orbit_ir::Expr::literal(serde_json::Value::Bool(false));
                }
            }
            let (t, l) = cck.check(c, None);
            if assignable(&t, &Type::Bool.optional()).is_err() {
                cck.diags.push(Diagnostic::error(
                    "E0408",
                    format!("`when` condition must be `Bool`, found `{t}`"),
                    c.span(),
                ));
            }
            l
        });
        let mut ck = Checker { env: &env, diags };
        let action = match &s.action {
            Some(StepAction::Worker(task, _)) => {
                let contract = step_contracts.get(name).copied().flatten();
                if let Some(c) = contract {
                    used_contracts.insert(c.name.clone(), c.clone());
                }
                let inv = lower_invocation(task, contract, &s.body, &mut ck, s.name.span);
                Some(Action::Task(inv))
            }
            Some(StepAction::Workflow(child, _)) => {
                let contract = step_contracts.get(name).copied().flatten();
                if let Some(c) = contract {
                    used_contracts.insert(c.name.clone(), c.clone());
                }
                // A child workflow is durable on its own: it is not retried,
                // routed by language, or keyed by an idempotency key.
                let mut bad = |span: Span, member: &str| {
                    ck.diags.push(
                        Diagnostic::error(
                            "E0307",
                            format!("`{member}` is not allowed in a `workflow` step"),
                            span,
                        )
                        .with_note(
                            "child workflows are durable; put retries on the child's own steps",
                        ),
                    );
                };
                if let Some(l) = &s.body.language {
                    bad(l.span, "using");
                }
                if let Some(r) = &s.body.retry {
                    bad(r.span, "retry");
                }
                if let Some(k) = &s.body.idempotency_key {
                    bad(k.span(), "idempotency_key");
                }
                if let Some((_, sp)) = s.body.queue_timeout {
                    bad(sp, "queue_timeout");
                }
                let task = format!("{WORKFLOW_CONTRACT_PREFIX}{child}");
                let inv = lower_invocation(&task, contract, &s.body, &mut ck, s.name.span);
                Some(Action::Workflow(inv))
            }
            Some(StepAction::Sleep(ms, _)) => {
                reject_members(&s.body, "a `sleep` step", false, ck.diags);
                Some(Action::Sleep { duration_ms: *ms })
            }
            Some(StepAction::Await(sig)) => {
                // `timeout` bounds the wait; nothing else applies.
                let mut body = s.body.clone();
                let timeout_ms = body.timeout.take().map(|t| t.0);
                reject_members(&body, "an `await` step", false, ck.diags);
                if timeout_ms.is_none()
                    && wf.timeout.is_none()
                    && signal_types.contains_key(&sig.name)
                {
                    ck.diags.push(
                        Diagnostic::warning(
                            "W0606",
                            format!(
                                "step `{name}` waits for signal `{}` with no timeout",
                                sig.name
                            ),
                            sig.span,
                        )
                        .with_help("add `timeout 7d` to the step (or a workflow `timeout`) so the execution cannot wait forever"),
                    );
                }
                Some(Action::Await {
                    signal: sig.name.clone(),
                    timeout_ms,
                })
            }
            Some(StepAction::Compute(e, _)) => {
                reject_members(&s.body, "a `compute` step", false, ck.diags);
                if let Some((_, span)) = s.body.max_parallel {
                    ck.diags.push(
                        Diagnostic::error("E0307", "`max_parallel` is not allowed in a `compute` step", span)
                            .with_note("compute elements are evaluated by the runtime in one pass; there is nothing to limit"),
                    );
                }
                let (t, l) = ck.check(e, None);
                computed_type = Some(t);
                Some(Action::Compute { expr: l })
            }
            None => None,
        };

        // Compensation: may read this step (non-optional, it succeeded) and its ancestors.
        let compensation = s.compensation.as_ref().and_then(|c| {
            let nothing = match s.action {
                Some(StepAction::Sleep(..)) => Some("a `sleep` step"),
                Some(StepAction::Await(..)) => Some("an `await` step"),
                Some(StepAction::Compute(..)) => Some("a `compute` step"),
                _ => None,
            };
            if let Some(what) = nothing {
                diags.push(Diagnostic::error(
                    "E0308",
                    format!("{what} has nothing to compensate"),
                    c.span,
                ));
                return None;
            }
            let mut allowed = ancestors(name);
            allowed.insert(name.to_string());
            let mut comp_types = step_types.clone();
            if let Some(Some(contract)) = step_contracts.get(name) {
                comp_types.insert(name.to_string(), contract.output.clone());
            }
            let why = format!(
                "a compensation for `{name}` can only read `{name}` and the steps it depends on"
            );
            let env = Env {
                input: Some(&input_ty),
                steps: &comp_types,
                allowed: Some((&allowed, &why)),
                context: "a compensation",
                locals: &locals,
            };
            let mut ck = Checker { env: &env, diags };
            reject_members(&c.body, "a `compensate` block", true, ck.diags);
            match &c.action {
                Some(StepAction::Worker(task, span)) => {
                    let contract = resolve_task(task, *span, ck.diags);
                    if let Some(ct) = contract {
                        used_contracts.insert(ct.name.clone(), ct.clone());
                    }
                    Some(lower_invocation(task, contract, &c.body, &mut ck, c.span))
                }
                Some(StepAction::Sleep(_, span))
                | Some(StepAction::Workflow(_, span))
                | Some(StepAction::Compute(_, span)) => {
                    ck.diags.push(Diagnostic::error(
                        "E0308",
                        "compensation must call a worker task",
                        *span,
                    ));
                    None
                }
                Some(StepAction::Await(sig)) => {
                    ck.diags.push(Diagnostic::error(
                        "E0308",
                        "compensation must call a worker task",
                        sig.span,
                    ));
                    None
                }
                None => {
                    ck.diags.push(Diagnostic::error(
                        "E0308",
                        "`compensate` block needs a `worker`",
                        c.span,
                    ));
                    None
                }
            }
        });

        if let Some(t) = computed_type {
            let t = if s.body.for_each.is_some() {
                Type::List { item: Box::new(t) }
            } else {
                t
            };
            let t = if s.body.condition.is_some() {
                t.optional()
            } else {
                t
            };
            step_types.insert(name.to_string(), t);
        }

        // Static safety: tasks that declare `compensated_by` must be compensated.
        if let (Some(StepAction::Worker(task, task_span)), Some(Some(contract))) =
            (&s.action, step_contracts.get(name))
        {
            if let Some(by) = &contract.compensated_by {
                let used = s.compensation.as_ref().and_then(|c| match &c.action {
                    Some(StepAction::Worker(t, sp)) => Some((t, *sp)),
                    _ => None,
                });
                match used {
                    None if s.compensation.is_none() => diags.push(
                        Diagnostic::warning(
                            "W0601",
                            format!("step `{name}` calls `{task}` but has no compensation"),
                            *task_span,
                        )
                        .with_note(format!("`{task}` declares `compensated_by \"{by}\"`: if a later step fails, its effect would be left in place"))
                        .with_help(format!("add `compensate {{ worker \"{by}\" with {{ ... }} }}`")),
                    ),
                    Some((t, sp)) if t != by => diags.push(
                        Diagnostic::warning(
                            "W0602",
                            format!("step `{name}` compensates `{task}` with `{t}`, but the contract names `{by}`"),
                            sp,
                        )
                        .with_help(format!("use `worker \"{by}\"` in the compensation, or change the contract's `compensated_by`")),
                    ),
                    _ => {}
                }
            }
        }
        // Static safety: a condition that is constant can be decided now.
        if let (Some(c), Some(cond_ir)) = (&s.body.condition, &condition) {
            if let Some(v) = constant_value(cond_ir) {
                if v == serde_json::Value::Bool(false) || v.is_null() {
                    diags.push(
                        Diagnostic::warning(
                            "W0603",
                            format!("step `{name}` never runs: its `when` condition is always false"),
                            c.span(),
                        )
                        .with_help("remove the step, or make the condition depend on input or step results"),
                    );
                } else if v == serde_json::Value::Bool(true) {
                    diags.push(
                        Diagnostic::warning(
                            "W0604",
                            format!("the `when` condition of step `{name}` is always true"),
                            c.span(),
                        )
                        .with_help(
                            "remove `when`; it also makes the step's output optional for no reason",
                        ),
                    );
                }
            }
        }

        if let Some(action) = action {
            let mut dependencies: Vec<String> = deps[name].keys().map(|d| d.to_string()).collect();
            dependencies.sort();
            steps_ir.push(StepIr {
                id: name.to_string(),
                action,
                dependencies,
                condition,
                for_each,
                compensation,
                output_type: step_types[*name].clone(),
            });
        }
    }

    // --- Result.
    let env = Env {
        input: Some(&input_ty),
        steps: &step_types,
        allowed: None,
        context: "`return`",
        locals: &[],
    };
    let mut ck = Checker { env: &env, diags };
    let (output, result) = match (&declared_output, &wf.ret) {
        (Some(out), Some(ret)) => {
            let (_, l) = ck.check(ret, Some(out));
            (out.clone(), Some(l))
        }
        (Some(out), None) => {
            if matches!(out, Type::Record { fields, .. } if fields.iter().any(FieldType::required))
            {
                let span = wf.output.as_ref().unwrap().1;
                ck.diags.push(
                    Diagnostic::error(
                        "E0501",
                        "workflow declares an `output` but has no `return`",
                        span,
                    )
                    .with_help("add `return { ... }` mapping step results to the output fields"),
                );
            }
            (out.clone(), None)
        }
        (None, Some(ret)) => {
            let (t, l) = ck.check(ret, None);
            (t, Some(l))
        }
        (None, None) => (Type::record(None, vec![]), None),
    };

    // --- continue_as_new: the next run's input is typed like a start input.
    let continue_as_new = wf.continue_as_new.as_ref().and_then(|can| {
        let env = Env {
            input: Some(&input_ty),
            steps: &step_types,
            allowed: None,
            context: "`continue_as_new`",
            locals: &[],
        };
        let mut ck = Checker {
            env: &env,
            diags: &mut *diags,
        };
        let condition = can.condition.as_ref().map(|c| {
            let (t, l) = ck.check(c, None);
            if assignable(&t, &Type::Bool.optional()).is_err() {
                ck.diags.push(Diagnostic::error(
                    "E0324",
                    format!("the `continue_as_new` condition must be `Bool`, found `{t}`"),
                    c.span(),
                ));
            }
            l
        });
        let input = can.input.as_ref().map(|i| ck.check(i, Some(&input_ty)).1)?;
        if condition.is_none() {
            if let Some(ret) = &wf.ret {
                ck.diags.push(
                    Diagnostic::warning(
                        "W0607",
                        "`return` is never used: `continue_as_new` has no `when`, so every run continues",
                        ret.span(),
                    )
                    .with_label(can.span, "always continues")
                    .with_help("add `when <condition>` to `continue_as_new`, or remove `return`"),
                );
            }
        }
        Some(ir::ContinueAsNewIr { condition, input })
    });

    // --- Queries: any step may not have run yet, so every step output is optional.
    let query_steps: BTreeMap<String, Type> = step_types
        .iter()
        .map(|(k, t)| (k.clone(), t.clone().optional()))
        .collect();
    let mut queries: BTreeMap<String, QueryIr> = BTreeMap::new();
    let mut query_spans: BTreeMap<&str, Span> = BTreeMap::new();
    for q in &wf.queries {
        if let Some(prev) = query_spans.get(q.name.name.as_str()) {
            diags.push(
                Diagnostic::error(
                    "E0318",
                    format!("query `{}` is declared more than once", q.name.name),
                    q.name.span,
                )
                .with_label(*prev, "first declared here"),
            );
            continue;
        }
        query_spans.insert(&q.name.name, q.name.span);
        let env = Env {
            input: Some(&input_ty),
            steps: &query_steps,
            allowed: None,
            context: "a query",
            locals: &[],
        };
        let (t, l) = Checker {
            env: &env,
            diags: &mut *diags,
        }
        .check(&q.expr, None);
        queries.insert(
            q.name.name.clone(),
            QueryIr {
                expr: l,
                output_type: t,
            },
        );
    }
    // Static safety: a signal nobody awaits is either dead or a missing step.
    for sig in &wf.signals {
        let awaited =
            wf.steps.iter().any(
                |s| matches!(&s.action, Some(StepAction::Await(a)) if a.name == sig.name.name),
            ) || wf
                .handlers
                .iter()
                .any(|h| h.event.as_ref().is_some_and(|e| e.name == sig.name.name));
        if !awaited {
            diags.push(
                Diagnostic::warning(
                    "W0605",
                    format!(
                        "signal `{}` is declared but no step awaits it",
                        sig.name.name
                    ),
                    sig.name.span,
                )
                .with_help(format!(
                    "add a step with `await {}`, or remove the declaration",
                    sig.name.name
                )),
            );
        }
    }

    // --- Per-workflow resource limits.
    let limits = wf.limits.as_ref().map(|block| {
        let mut l = ir::WorkflowLimits::default();
        for (key, expr) in &block.members {
            let value = match expr {
                ast::Expr::Int(n, _) if *n > 0 => Some(*n as u64),
                _ => {
                    diags.push(
                        Diagnostic::error(
                            "E0321",
                            format!("limit `{}` must be a positive integer literal", key.name),
                            expr.span(),
                        )
                        .with_help("for example `max_fan_out: 100`"),
                    );
                    None
                }
            };
            let slot = match key.name.as_str() {
                "max_fan_out" => &mut l.max_fan_out,
                "max_history_events" => &mut l.max_history_events,
                "max_payload_bytes" => &mut l.max_payload_bytes,
                other => {
                    let mut d =
                        Diagnostic::error("E0320", format!("unknown limit `{other}`"), key.span);
                    if let Some(sug) = suggest(other, ir::WORKFLOW_LIMITS.iter().copied()) {
                        d = d.with_help(format!("did you mean `{sug}`?"));
                    } else {
                        d = d.with_note(format!("limits: {}", ir::WORKFLOW_LIMITS.join(", ")));
                    }
                    diags.push(d);
                    continue;
                }
            };
            if slot.is_some() {
                diags.push(Diagnostic::error(
                    "E0106",
                    format!("duplicate limit `{}`", key.name),
                    key.span,
                ));
            }
            *slot = value;
        }
        l
    });

    // --- Outcome handlers (`on failure|cancel|timeout`).
    let (handlers, events) = lower_handlers(
        wf,
        &step_types,
        &signal_types,
        &input_ty,
        contracts,
        &mut used_contracts,
        diags,
    );

    let errors = diags.iter().filter(|d| d.is_error()).count();
    if errors > start_errors || missing_child {
        return None;
    }
    let mut ir = WorkflowIr {
        ir_version: ir::IR_VERSION.into(),
        name: wf.name.name.clone(),
        version,
        description: wf.description.clone(),
        definition_hash: String::new(),
        timeout_ms: wf.timeout.map(|t| t.0),
        input: input_ty,
        output,
        steps: steps_ir,
        result,
        contracts: used_contracts,
        signals: signal_types,
        queries,
        handlers,
        limits,
        events,
        continue_as_new,
        source_map,
    };
    ir.ir_version = ir.required_ir_version().into();
    let ir = ir.seal();
    debug_assert!(ir.validate().is_ok(), "{:?}", ir.validate());
    Some(ir)
}

/// The type of `failure` inside `on` handlers.
pub fn failure_type() -> Type {
    Type::record(
        Some("Failure"),
        vec![
            FieldType {
                name: "reason".into(),
                ty: Type::String,
                default: None,
            },
            FieldType {
                name: "code".into(),
                ty: Type::String,
                default: None,
            },
            FieldType {
                name: "message".into(),
                ty: Type::String,
                default: None,
            },
        ],
    )
}

/// Lower `on failure|cancel|timeout { step ... }` blocks. Handler steps may
/// read the workflow input, `failure`, the outputs of main steps (optional:
/// they may not have run) and earlier handler steps of the same block.
#[allow(clippy::type_complexity)]
fn lower_handlers(
    wf: &ast::Workflow,
    step_types: &BTreeMap<String, Type>,
    signal_types: &BTreeMap<String, Type>,
    input_ty: &Type,
    contracts: &BTreeMap<String, TaskContract>,
    used_contracts: &mut BTreeMap<String, TaskContract>,
    diags: &mut Vec<Diagnostic>,
) -> (BTreeMap<String, Vec<StepIr>>, BTreeMap<String, Vec<StepIr>>) {
    let mut out: BTreeMap<String, Vec<StepIr>> = BTreeMap::new();
    let mut events: BTreeMap<String, Vec<StepIr>> = BTreeMap::new();
    let mut seen_kinds: BTreeMap<String, Span> = BTreeMap::new();
    let failure = failure_type();
    for h in &wf.handlers {
        // `on event <signal>`: the signal must exist and must not also be awaited.
        let event_signal = h.event.as_ref();
        let label = match event_signal {
            Some(sig) => format!("event {}", sig.name),
            None => h.kind.name.clone(),
        };
        if let Some(prev) = seen_kinds.get(&label) {
            diags.push(
                Diagnostic::error(
                    "E0319",
                    format!("`on {label}` is declared more than once"),
                    h.kind.span,
                )
                .with_label(*prev, "first declared here"),
            );
            continue;
        }
        seen_kinds.insert(label.clone(), h.kind.span);
        let local = match event_signal {
            Some(sig) => match signal_types.get(&sig.name) {
                Some(t) => {
                    for s in &wf.steps {
                        if let Some(StepAction::Await(a)) = &s.action {
                            if a.name == sig.name {
                                diags.push(
                                    Diagnostic::error(
                                        "E0322",
                                        format!("signal `{}` is handled by `on event` and cannot also be awaited", sig.name),
                                        a.span,
                                    )
                                    .with_label(sig.span, "handled here")
                                    .with_note("every signal handled by `on event` starts the handler; an `await` would compete for it"),
                                );
                            }
                        }
                    }
                    ("event".to_string(), t.clone())
                }
                None => {
                    let mut d = Diagnostic::error(
                        "E0315",
                        format!("unknown signal `{}`", sig.name),
                        sig.span,
                    );
                    if let Some(sug) = suggest(&sig.name, signal_types.keys().map(String::as_str)) {
                        d = d.with_help(format!("did you mean `{sug}`?"));
                    } else {
                        d = d.with_help(format!(
                            "declare it in the workflow: `signal {} {{ ... }}`",
                            sig.name
                        ));
                    }
                    diags.push(d);
                    continue;
                }
            },
            None => ("failure".to_string(), failure.clone()),
        };
        // Names: unique in the block, distinct from main steps.
        let mut by_name: BTreeMap<&str, &ast::Step> = BTreeMap::new();
        for s in &h.steps {
            let n = s.name.name.as_str();
            if step_types.contains_key(n)
                || RESERVED_STEP_NAMES.contains(&n)
                || n == "failure"
                || n == "event"
            {
                diags.push(
                    Diagnostic::error(
                        "E0302",
                        format!("handler step `{n}` has the same name as a workflow step or a reserved name"),
                        s.name.span,
                    )
                    .with_help("give handler steps their own names, e.g. `alert_ops`"),
                );
                continue;
            }
            if let Some(prev) = by_name.get(n) {
                diags.push(
                    Diagnostic::error(
                        "E0302",
                        format!("step `{n}` is defined more than once"),
                        s.name.span,
                    )
                    .with_label(prev.name.span, "first defined here"),
                );
                continue;
            }
            by_name.insert(n, s);
        }
        let names: HashSet<&str> = by_name.keys().copied().collect();
        // Dependencies within the block.
        let mut deps: BTreeMap<&str, BTreeMap<&str, Span>> = BTreeMap::new();
        for (name, s) in &by_name {
            let entry = deps.entry(name).or_default();
            for a in &s.body.after {
                match names.get(a.name.as_str()) {
                    Some(k) if *k != *name => {
                        entry.insert(k, a.span);
                    }
                    _ => {
                        diags.push(
                            Diagnostic::error(
                                "E0303",
                                format!("`after` in an `on {label}` block must name a step of the same block"),
                                a.span,
                            )
                            .with_note("workflow steps have all finished before a handler runs"),
                        );
                    }
                }
            }
            let mut refs = vec![];
            let computed = match &s.action {
                Some(StepAction::Compute(e, _)) => Some(e),
                _ => None,
            };
            for e in [
                s.body.with.as_ref(),
                s.body.condition.as_ref(),
                s.body.idempotency_key.as_ref(),
                computed,
            ]
            .into_iter()
            .flatten()
            {
                step_refs(e, &names, &mut refs);
            }
            for r in refs {
                if r.name != **name {
                    let key = by_name.get_key_value(r.name.as_str()).unwrap().0;
                    entry.entry(key).or_insert(r.span);
                }
            }
        }
        if let Some(cycle) = find_cycle(&deps) {
            diags.push(
                Diagnostic::error(
                    "E0304",
                    "circular dependency between handler steps",
                    by_name[cycle[0]].name.span,
                )
                .with_note(format!("cycle: {}", cycle.join(" -> "))),
            );
            continue;
        }
        let mut order: Vec<&str> = vec![];
        let source_order: Vec<&str> = h
            .steps
            .iter()
            .map(|s| s.name.name.as_str())
            .filter(|n| by_name.contains_key(n))
            .collect();
        while order.len() < source_order.len() {
            let next = source_order
                .iter()
                .find(|n| !order.contains(n) && deps[*n].keys().all(|d| order.contains(d)))
                .copied()
                .expect("acyclic");
            order.push(next);
        }
        // Types: main steps are optional here (they may not have run).
        let mut types: BTreeMap<String, Type> = step_types
            .iter()
            .map(|(k, t)| (k.clone(), t.clone().optional()))
            .collect();
        let locals = vec![local];
        let mut steps_ir = vec![];
        for name in order {
            let s = by_name[name];
            let what = format!("an `on {label}` step");
            let bad = |diags: &mut Vec<Diagnostic>, span: Span, member: &str| {
                diags.push(Diagnostic::error(
                    "E0307",
                    format!("`{member}` is not allowed in {what}"),
                    span,
                ));
            };
            if let Some(f) = &s.body.for_each {
                bad(diags, f.span, "for");
            }
            if let Some((_, span)) = s.body.max_parallel {
                bad(diags, span, "max_parallel");
            }
            if let Some(c) = &s.compensation {
                diags.push(Diagnostic::error(
                    "E0308",
                    "handler steps cannot declare `compensate`",
                    c.span,
                ));
            }
            let env = Env {
                input: Some(input_ty),
                steps: &types,
                allowed: None,
                context: "a handler",
                locals: &locals,
            };
            let condition = s.body.condition.as_ref().map(|c| {
                let mut ck = Checker {
                    env: &env,
                    diags: &mut *diags,
                };
                let (t, l) = ck.check(c, None);
                if assignable(&t, &Type::Bool.optional()).is_err() {
                    ck.diags.push(Diagnostic::error(
                        "E0408",
                        format!("`when` condition must be `Bool`, found `{t}`"),
                        c.span(),
                    ));
                }
                l
            });
            let mut ck = Checker {
                env: &env,
                diags: &mut *diags,
            };
            let (action, out_ty) = match &s.action {
                Some(StepAction::Worker(task, span)) => match contracts.get(task) {
                    Some(c) => {
                        used_contracts.insert(c.name.clone(), c.clone());
                        let inv = lower_invocation(task, Some(c), &s.body, &mut ck, s.name.span);
                        (Some(Action::Task(inv)), c.output.clone())
                    }
                    None => {
                        let mut d = Diagnostic::error(
                            "E0300",
                            format!("unknown worker task `{task}`"),
                            *span,
                        );
                        if let Some(sug) = suggest(task, contracts.keys().map(String::as_str)) {
                            d = d.with_help(format!("did you mean `{sug}`?"));
                        }
                        ck.diags.push(d);
                        (None, Type::Json)
                    }
                },
                Some(StepAction::Compute(e, _)) => {
                    reject_members(&s.body, "a `compute` step", false, ck.diags);
                    let (t, l) = ck.check(e, None);
                    (Some(Action::Compute { expr: l }), t)
                }
                Some(StepAction::Sleep(ms, _)) => {
                    reject_members(&s.body, "a `sleep` step", false, ck.diags);
                    (
                        Some(Action::Sleep { duration_ms: *ms }),
                        Type::record(None, vec![]),
                    )
                }
                Some(StepAction::Await(sig)) => {
                    ck.diags.push(
                        Diagnostic::error("E0307", format!("`await` is not allowed in {what}"), sig.span)
                            .with_note("handlers run while the execution is finishing; it no longer accepts signals"),
                    );
                    (None, Type::Json)
                }
                Some(StepAction::Workflow(_, span)) => {
                    ck.diags.push(Diagnostic::error(
                        "E0307",
                        format!("child workflows are not allowed in {what}"),
                        *span,
                    ));
                    (None, Type::Json)
                }
                None => {
                    ck.diags.push(Diagnostic::error(
                        "E0301",
                        format!("step `{name}` does nothing"),
                        s.name.span,
                    ));
                    (None, Type::Json)
                }
            };
            let out_ty = if s.body.condition.is_some() {
                out_ty.optional()
            } else {
                out_ty
            };
            types.insert(name.to_string(), out_ty.clone());
            if let Some(action) = action {
                let mut dependencies: Vec<String> =
                    deps[name].keys().map(|d| d.to_string()).collect();
                dependencies.sort();
                steps_ir.push(StepIr {
                    id: name.to_string(),
                    action,
                    dependencies,
                    condition,
                    for_each: None,
                    compensation: None,
                    output_type: out_ty,
                });
            }
        }
        match event_signal {
            Some(sig) => {
                events.insert(sig.name.clone(), steps_ir);
            }
            None => {
                out.insert(h.kind.name.clone(), steps_ir);
            }
        }
    }
    (out, events)
}

/// The value of an IR expression that reads nothing (no input, steps,
/// execution metadata or variables), if it evaluates without error.
fn constant_value(e: &orbit_ir::Expr) -> Option<serde_json::Value> {
    fn reads_state(e: &orbit_ir::Expr) -> bool {
        use orbit_ir::Expr as E;
        match e {
            E::Input { .. } | E::Step { .. } | E::Execution { .. } | E::Local { .. } => true,
            E::Literal { .. } => false,
            E::Index { target, index } => reads_state(target) || reads_state(index),
            E::Object { fields } => fields.iter().any(|f| reads_state(&f.value)),
            E::List { items } | E::Call { args: items, .. } => items.iter().any(reads_state),
            E::Binary { left, right, .. } => reads_state(left) || reads_state(right),
            E::Unary { operand, .. } => reads_state(operand),
            E::If {
                condition,
                then,
                otherwise,
            } => reads_state(condition) || reads_state(then) || reads_state(otherwise),
            E::Lambda { .. } => true,
        }
    }
    if reads_state(e) {
        return None;
    }
    struct Empty(serde_json::Value);
    impl orbit_ir::Scope for Empty {
        fn input(&self) -> &serde_json::Value {
            &self.0
        }
        fn step_output(&self, _: &str) -> Option<&serde_json::Value> {
            None
        }
        fn execution_field(&self, _: &str) -> Option<serde_json::Value> {
            None
        }
    }
    orbit_ir::eval(e, &Empty(serde_json::Value::Null)).ok()
}

fn reject_members(
    body: &ast::StepBody,
    what: &str,
    allow_task_members: bool,
    diags: &mut Vec<Diagnostic>,
) {
    let mut bad = |span: Span, member: &str| {
        diags.push(Diagnostic::error(
            "E0307",
            format!("`{member}` is not allowed in {what}"),
            span,
        ));
    };
    if allow_task_members {
        if let Some(c) = &body.condition {
            bad(c.span(), "when");
        }
        if let Some(f) = &body.for_each {
            bad(f.span, "for");
        }
        if let Some((_, span)) = body.max_parallel {
            bad(span, "max_parallel");
        }
        if let Some(a) = body.after.first() {
            bad(a.span, "after");
        }
        return;
    }
    if let Some(l) = &body.language {
        bad(l.span, "using");
    }
    if let Some(w) = &body.with {
        bad(w.span(), "with");
    }
    if let Some((_, s)) = body.timeout {
        bad(s, "timeout");
    }
    if let Some((_, s)) = body.queue_timeout {
        bad(s, "queue_timeout");
    }
    if let Some(r) = &body.retry {
        bad(r.span, "retry");
    }
    if let Some(k) = &body.idempotency_key {
        bad(k.span(), "idempotency_key");
    }
}

fn lower_invocation(
    task: &str,
    contract: Option<&TaskContract>,
    body: &ast::StepBody,
    ck: &mut Checker,
    owner_span: Span,
) -> Invocation {
    let input = match (&body.with, contract) {
        (Some(w), Some(c)) => ck.check(w, Some(&c.input)).1,
        (Some(w), None) => ck.check(w, None).1,
        (None, Some(c)) => {
            let missing: Vec<String> = match &c.input {
                Type::Record { fields, .. } => fields
                    .iter()
                    .filter(|f| f.required())
                    .map(|f| format!("`{}: {}`", f.name, f.ty))
                    .collect(),
                _ => vec![],
            };
            if !missing.is_empty() {
                ck.diags.push(
                    Diagnostic::error(
                        "E0403",
                        format!("task `{task}` requires input but the step has no `with`"),
                        owner_span,
                    )
                    .with_note(format!("missing: {}", missing.join(", ")))
                    .with_help("add `with { ... }` providing the task's input fields"),
                );
            }
            ir::Expr::Object { fields: vec![] }
        }
        (None, None) => ir::Expr::Object { fields: vec![] },
    };
    let idempotency_key = body
        .idempotency_key
        .as_ref()
        .map(|k| ck.check(k, Some(&Type::String)).1);
    if let Some(lang) = &body.language {
        if !KNOWN_LANGUAGES.contains(&lang.name.as_str()) {
            let mut d = Diagnostic::warning(
                "W0301",
                format!("unknown worker language `{}`", lang.name),
                lang.span,
            );
            if let Some(s) = suggest(&lang.name, KNOWN_LANGUAGES.iter().copied()) {
                d = d.with_help(format!("did you mean `{s}`?"));
            } else {
                d = d.with_note("tasks are only routed to workers that announce this runtime");
            }
            ck.diags.push(d);
        }
    }
    for (value, span) in [body.timeout, body.queue_timeout].into_iter().flatten() {
        if value == 0 {
            ck.diags.push(Diagnostic::error(
                "E0410",
                "timeouts must be greater than zero",
                span,
            ));
        }
    }
    Invocation {
        task: task.to_string(),
        contract_version: contract.map(|c| c.version.clone()).unwrap_or_default(),
        language: body.language.as_ref().map(|l| l.name.clone()),
        input,
        idempotency_key,
        timeout_ms: body.timeout.map(|t| t.0),
        queue_timeout_ms: body.queue_timeout.map(|t| t.0),
        retry: body
            .retry
            .as_ref()
            .map(|r| lower_retry(r, ck.diags))
            .unwrap_or_default(),
    }
}

fn lower_retry(r: &ast::Retry, diags: &mut Vec<Diagnostic>) -> RetryPolicy {
    const KEYS: &[&str] = &[
        "attempts",
        "backoff",
        "initial_delay",
        "max_delay",
        "non_retryable",
    ];
    let mut p = RetryPolicy::default();
    let mut max_set = false;
    let mut seen: HashSet<&str> = HashSet::new();
    for (k, v) in &r.members {
        if !seen.insert(k.name.as_str()) {
            diags.push(Diagnostic::error(
                "E0410",
                format!("duplicate retry setting `{}`", k.name),
                k.span,
            ));
            continue;
        }
        let bad = |diags: &mut Vec<Diagnostic>, want: &str| {
            diags.push(Diagnostic::error(
                "E0410",
                format!("`{}` must be {want}", k.name),
                v.span(),
            ));
        };
        match (k.name.as_str(), v) {
            ("attempts", ast::Expr::Int(n, span)) => {
                if *n < 1 || *n > 1000 {
                    diags.push(
                        Diagnostic::error("E0410", "`attempts` must be between 1 and 1000", *span)
                            .with_note("`attempts` counts every execution, including the first"),
                    );
                } else {
                    p.max_attempts = *n as u32;
                }
            }
            ("attempts", _) => bad(diags, "an integer literal"),
            ("backoff", ast::Expr::Path(segs, span)) if segs.len() == 1 => {
                match segs[0].name.as_str() {
                    "fixed" => p.backoff = Backoff::Fixed,
                    "exponential" => p.backoff = Backoff::Exponential,
                    other => {
                        let mut d = Diagnostic::error(
                            "E0410",
                            format!("unknown backoff strategy `{other}`"),
                            *span,
                        );
                        if let Some(s) = suggest(other, ["fixed", "exponential"]) {
                            d = d.with_help(format!("did you mean `{s}`?"));
                        } else {
                            d = d.with_note("supported strategies: fixed, exponential");
                        }
                        diags.push(d);
                    }
                }
            }
            ("backoff", _) => bad(diags, "`fixed` or `exponential`"),
            ("initial_delay", ast::Expr::Duration(ms, _)) => p.initial_delay_ms = *ms,
            ("max_delay", ast::Expr::Duration(ms, _)) => {
                p.max_delay_ms = *ms;
                max_set = true;
            }
            ("initial_delay" | "max_delay", _) => bad(diags, "a duration literal such as `5s`"),
            ("non_retryable", ast::Expr::List(items, _)) => {
                for i in items {
                    match i {
                        ast::Expr::Str(s, _) => p.non_retryable.push(s.clone()),
                        other => diags.push(Diagnostic::error(
                            "E0410",
                            "error codes must be string literals",
                            other.span(),
                        )),
                    }
                }
            }
            ("non_retryable", _) => bad(diags, "a list of error code strings"),
            (other, _) => {
                let mut d =
                    Diagnostic::error("E0410", format!("unknown retry setting `{other}`"), k.span);
                if let Some(s) = suggest(other, KEYS.iter().copied()) {
                    d = d.with_help(format!("did you mean `{s}`?"));
                } else {
                    d = d.with_note(format!("valid settings: {}", KEYS.join(", ")));
                }
                diags.push(d);
            }
        }
    }
    if !max_set {
        p.max_delay_ms = p.max_delay_ms.max(p.initial_delay_ms);
    } else if p.max_delay_ms < p.initial_delay_ms {
        diags.push(Diagnostic::error(
            "E0410",
            "`max_delay` must be at least `initial_delay`",
            r.span,
        ));
    }
    p
}

fn find_cycle<'a>(deps: &BTreeMap<&'a str, BTreeMap<&'a str, Span>>) -> Option<Vec<&'a str>> {
    #[derive(Clone, Copy, PartialEq)]
    enum Mark {
        Visiting,
        Done,
    }
    fn visit<'a>(
        n: &'a str,
        deps: &BTreeMap<&'a str, BTreeMap<&'a str, Span>>,
        marks: &mut HashMap<&'a str, Mark>,
        stack: &mut Vec<&'a str>,
    ) -> Option<Vec<&'a str>> {
        match marks.get(n) {
            Some(Mark::Done) => return None,
            Some(Mark::Visiting) => {
                let start = stack.iter().position(|s| *s == n).unwrap();
                let mut cycle = stack[start..].to_vec();
                cycle.push(n);
                return Some(cycle);
            }
            None => {}
        }
        marks.insert(n, Mark::Visiting);
        stack.push(n);
        for d in deps
            .get(n)
            .map(|m| m.keys().copied().collect::<Vec<_>>())
            .unwrap_or_default()
        {
            if let Some(c) = visit(d, deps, marks, stack) {
                return Some(c);
            }
        }
        stack.pop();
        marks.insert(n, Mark::Done);
        None
    }
    let mut marks = HashMap::new();
    for n in deps.keys() {
        if let Some(c) = visit(n, deps, &mut marks, &mut vec![]) {
            return Some(c);
        }
    }
    None
}
