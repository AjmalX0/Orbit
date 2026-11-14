//! The ORBIT compiler.
//!
//! Stages (RFC-0001 §12.1): source loading → lexing → parsing → symbol
//! resolution → type checking → dependency graph construction → workflow
//! validation → typed IR generation.

pub mod check;
pub mod prelude;
pub mod types;

use check::{Checker, Env};
use orbit_ir::{
    self as ir, Action, Backoff, FieldType, Invocation, RetryPolicy, SourceLocation, StepIr,
    TaskContract, Type, WorkflowIr,
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
    };
    analyze(&mut comp, prelude_id, &programs);
    if comp.has_errors() {
        comp.workflows.clear();
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
                Declaration::Import(_) => {}
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
            },
        );
    }

    for (file, wf) in workflows {
        if let Some(ir) = lower_workflow(wf, &schemas, &contracts, &comp.sources, diags) {
            comp.workflows.push(ir);
            comp.workflow_files.push(file);
        }
    }
    comp.contracts = contracts;
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

    let mut step_types: BTreeMap<String, Type> = BTreeMap::new();
    let mut step_contracts: HashMap<&str, Option<&TaskContract>> = HashMap::new();
    for (name, s) in &by_name {
        let out = match &s.action {
            Some(StepAction::Worker(task, span)) => {
                let c = resolve_task(task, *span, diags);
                step_contracts.insert(name, c);
                c.map(|c| c.output.clone()).unwrap_or(Type::Json)
            }
            Some(StepAction::Sleep(..)) => Type::record(None, vec![]),
            None => {
                diags.push(
                    Diagnostic::error("E0301", format!("step `{name}` does nothing"), s.name.span)
                        .with_help("add `worker \"task.name\"` or `sleep 10s`"),
                );
                Type::Json
            }
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
        for e in [&s.body.with, &s.body.condition, &s.body.idempotency_key]
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
        d = d.with_help(
            "the step graph must be acyclic in ORBIT v0.1; remove one of these dependencies",
        );
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
        source_map.insert(name.to_string(), loc(s.name.span));
        let env = Env {
            input: Some(&input_ty),
            steps: &step_types,
            allowed: None,
            context: "a step",
        };
        let mut ck = Checker { env: &env, diags };
        let condition = s.body.condition.as_ref().map(|c| {
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
        let action = match &s.action {
            Some(StepAction::Worker(task, _)) => {
                let contract = step_contracts.get(name).copied().flatten();
                if let Some(c) = contract {
                    used_contracts.insert(c.name.clone(), c.clone());
                }
                let inv = lower_invocation(task, contract, &s.body, &mut ck, s.name.span);
                Some(Action::Task(inv))
            }
            Some(StepAction::Sleep(ms, _)) => {
                reject_members(&s.body, "a `sleep` step", false, ck.diags);
                Some(Action::Sleep { duration_ms: *ms })
            }
            None => None,
        };

        // Compensation: may read this step (non-optional, it succeeded) and its ancestors.
        let compensation = s.compensation.as_ref().and_then(|c| {
            if matches!(s.action, Some(StepAction::Sleep(..))) {
                diags.push(Diagnostic::error(
                    "E0308",
                    "a `sleep` step has nothing to compensate",
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
                Some(StepAction::Sleep(_, span)) => {
                    ck.diags.push(Diagnostic::error(
                        "E0308",
                        "compensation must call a worker, not `sleep`",
                        *span,
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

        if let Some(action) = action {
            let mut dependencies: Vec<String> = deps[name].keys().map(|d| d.to_string()).collect();
            dependencies.sort();
            steps_ir.push(StepIr {
                id: name.to_string(),
                action,
                dependencies,
                condition,
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

    let errors = diags.iter().filter(|d| d.is_error()).count();
    if errors > start_errors {
        return None;
    }
    let ir = WorkflowIr {
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
        source_map,
    }
    .seal();
    debug_assert!(ir.validate().is_ok(), "{:?}", ir.validate());
    Some(ir)
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
