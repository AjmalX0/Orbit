//! `orbit diff`: what changed in a business process between two versions.
//!
//! [`crate::compat`] answers "is this change safe and which version bump does
//! it need?"; this module answers "what does the process now do differently?":
//! steps added, removed or renamed, changed actions and contracts,
//! dependencies, conditions, retries, timeouts, compensations, handlers,
//! event blocks, signals, queries, types and limits. The compatibility verdict
//! is included so a single report can go into a pull request.
//!
//! Workflows are matched by name, steps by scope (main graph, `on <kind>`,
//! `on event <signal>`) and id. A removed step and an added step in the same
//! scope are reported as a rename when they are the only unmatched pair that
//! runs the same thing (the same task, child workflow or signal, or a `sleep`
//! or `compute` step); dependencies on a renamed step are compared under its
//! new name.

use super::pretty;
use super::{action_summary, retry_summary, scope_steps, scopes, timeout_summary, Scope};
use crate::compat::{self, Report, Snapshot};
use orbit_ir::{Action, StepIr, TaskContract, Type, WorkflowIr, WORKFLOW_CONTRACT_PREFIX};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Added,
    Removed,
    Changed,
    Renamed,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Added => "added",
            Kind::Removed => "removed",
            Kind::Changed => "changed",
            Kind::Renamed => "renamed",
        }
    }

    fn sigil(self) -> char {
        match self {
            Kind::Added => '+',
            Kind::Removed => '-',
            Kind::Changed => '~',
            Kind::Renamed => '>',
        }
    }
}

/// One change. `area` is one of: `workflow`, `input`, `output`, `signal`,
/// `query`, `step`, `action`, `step input`, `dependency`, `condition`,
/// `fan-out`, `retry`, `timeout`, `idempotency`, `compensation`, `handler`,
/// `event`, `limits`, `result`, `contract`.
#[derive(Debug, Clone, PartialEq)]
pub struct Change {
    pub kind: Kind,
    pub area: &'static str,
    /// What changed, e.g. `step charge`, `on failure: tell`, `input.amount`.
    pub subject: String,
    pub old: Option<String>,
    pub new: Option<String>,
}

impl Change {
    fn new(kind: Kind, area: &'static str, subject: impl Into<String>) -> Change {
        Change {
            kind,
            area,
            subject: subject.into(),
            old: None,
            new: None,
        }
    }

    fn values(mut self, old: Option<String>, new: Option<String>) -> Change {
        self.old = old;
        self.new = new;
        self
    }

    fn to_json(&self) -> Value {
        json!({
            "kind": self.kind.as_str(),
            "area": self.area,
            "subject": self.subject,
            "old": self.old,
            "new": self.new,
        })
    }

    fn text(&self) -> String {
        let mut s = format!("{} {}: {}", self.kind.sigil(), self.area, self.subject);
        match (&self.old, &self.new) {
            (Some(a), Some(b)) => {
                let _ = write!(s, ": {a} → {b}");
            }
            (Some(a), None) => {
                let _ = write!(s, ": {a}");
            }
            (None, Some(b)) => {
                let _ = write!(s, ": {b}");
            }
            (None, None) => {}
        }
        s
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Added,
    Removed,
    Changed,
    Unchanged,
}

#[derive(Debug, Clone)]
pub struct WorkflowDiff {
    pub name: String,
    pub old_version: Option<String>,
    pub new_version: Option<String>,
    pub status: Status,
    pub changes: Vec<Change>,
}

#[derive(Debug, Clone)]
pub struct Diff {
    pub workflows: Vec<WorkflowDiff>,
    /// Changes to declared task contracts (by name).
    pub contracts: Vec<Change>,
    pub compat: Report,
}

fn opt_str(s: Option<String>) -> String {
    s.unwrap_or_else(|| "none".into())
}

/// Compare two snapshots. With `only`, restrict the report (workflows and
/// compatibility findings) to that workflow.
pub fn diff(old: &Snapshot, new: &Snapshot, only: Option<&str>) -> Diff {
    let ow: BTreeMap<&str, &WorkflowIr> =
        old.workflows.iter().map(|w| (w.name.as_str(), w)).collect();
    let nw: BTreeMap<&str, &WorkflowIr> =
        new.workflows.iter().map(|w| (w.name.as_str(), w)).collect();
    let names: BTreeSet<&str> = ow.keys().chain(nw.keys()).copied().collect();
    let mut workflows = vec![];
    for name in names {
        if only.is_some_and(|o| o != name) {
            continue;
        }
        let wd = match (ow.get(name), nw.get(name)) {
            (Some(o), Some(n)) => {
                let changes = workflow_changes(o, n);
                WorkflowDiff {
                    name: name.to_string(),
                    old_version: Some(o.version.clone()),
                    new_version: Some(n.version.clone()),
                    status: if changes.is_empty() {
                        Status::Unchanged
                    } else {
                        Status::Changed
                    },
                    changes,
                }
            }
            (Some(o), None) => WorkflowDiff {
                name: name.to_string(),
                old_version: Some(o.version.clone()),
                new_version: None,
                status: Status::Removed,
                changes: vec![Change::new(Kind::Removed, "workflow", name)
                    .values(Some(o.version.clone()), None)],
            },
            (None, Some(n)) => WorkflowDiff {
                name: name.to_string(),
                old_version: None,
                new_version: Some(n.version.clone()),
                status: Status::Added,
                changes: vec![Change::new(Kind::Added, "workflow", name).values(
                    None,
                    Some(format!("{} ({} steps)", n.version, n.steps.len())),
                )],
            },
            (None, None) => unreachable!(),
        };
        workflows.push(wd);
    }
    let contracts = if only.is_some() {
        vec![]
    } else {
        contract_changes(old, new)
    };
    let mut compat = compat::compare(old, new);
    if let Some(o) = only {
        compat
            .findings
            .retain(|f| f.subject == compat::Subject::Contract || f.name == o);
    }
    Diff {
        workflows,
        contracts,
        compat,
    }
}

fn declared(s: &Snapshot) -> BTreeMap<String, TaskContract> {
    let mut out: BTreeMap<String, TaskContract> = BTreeMap::new();
    for c in s
        .contracts
        .values()
        .chain(s.workflows.iter().flat_map(|w| w.contracts.values()))
    {
        if c.builtin || c.name.starts_with(WORKFLOW_CONTRACT_PREFIX) {
            continue;
        }
        out.entry(c.name.clone()).or_insert_with(|| c.clone());
    }
    out
}

fn contract_changes(old: &Snapshot, new: &Snapshot) -> Vec<Change> {
    let (o, n) = (declared(old), declared(new));
    let mut out = vec![];
    let names: BTreeSet<&String> = o.keys().chain(n.keys()).collect();
    for name in names {
        match (o.get(name), n.get(name)) {
            (Some(a), Some(b)) => {
                if a.version != b.version {
                    out.push(
                        Change::new(Kind::Changed, "contract", format!("{name} version"))
                            .values(Some(a.version.clone()), Some(b.version.clone())),
                    );
                }
                if a.compensated_by != b.compensated_by {
                    out.push(
                        Change::new(Kind::Changed, "contract", format!("{name} compensated_by"))
                            .values(
                                Some(opt_str(a.compensated_by.clone())),
                                Some(opt_str(b.compensated_by.clone())),
                            ),
                    );
                }
                let mut tc = vec![];
                type_changes(&a.input, &b.input, &format!("{name} input"), &mut tc);
                type_changes(&a.output, &b.output, &format!("{name} output"), &mut tc);
                for mut c in tc {
                    c.area = "contract";
                    out.push(c);
                }
            }
            (Some(a), None) => out.push(
                Change::new(Kind::Removed, "contract", name.clone())
                    .values(Some(a.version.clone()), None),
            ),
            (None, Some(b)) => out.push(
                Change::new(Kind::Added, "contract", name.clone())
                    .values(None, Some(b.version.clone())),
            ),
            (None, None) => {}
        }
    }
    out
}

/// Field-level changes between two types: added, removed and retyped fields,
/// and changed defaults. Records are compared field by field (also inside
/// lists, maps and optionals); anything else is compared as a whole.
pub fn type_changes(old: &Type, new: &Type, path: &str, out: &mut Vec<Change>) {
    let area = if path.starts_with("input") {
        "input"
    } else if path.starts_with("output") {
        "output"
    } else if path.starts_with("signal") {
        "signal"
    } else if path.starts_with("query") {
        "query"
    } else {
        "contract"
    };
    match (old, new) {
        (Type::Record { fields: a, .. }, Type::Record { fields: b, .. }) => {
            for f in a {
                let p = format!("{path}.{}", f.name);
                match b.iter().find(|g| g.name == f.name) {
                    Some(g) => {
                        type_changes(&f.ty, &g.ty, &p, out);
                        if f.default != g.default {
                            let show = |d: &Option<Value>| {
                                d.as_ref()
                                    .map(pretty::literal)
                                    .unwrap_or_else(|| "no default".into())
                            };
                            out.push(
                                Change::new(Kind::Changed, area, format!("{p} default"))
                                    .values(Some(show(&f.default)), Some(show(&g.default))),
                            );
                        }
                    }
                    None => out.push(
                        Change::new(Kind::Removed, area, p).values(Some(f.ty.to_string()), None),
                    ),
                }
            }
            for g in b {
                if !a.iter().any(|f| f.name == g.name) {
                    out.push(
                        Change::new(Kind::Added, area, format!("{path}.{}", g.name))
                            .values(None, Some(g.ty.to_string())),
                    );
                }
            }
        }
        (Type::List { item: a }, Type::List { item: b }) => {
            type_changes(a, b, &format!("{path}[]"), out)
        }
        (Type::Optional { inner: a }, Type::Optional { inner: b }) => type_changes(a, b, path, out),
        (Type::Map { key: ka, value: a }, Type::Map { key: kb, value: b }) if ka == kb => {
            type_changes(a, b, &format!("{path}{{}}"), out)
        }
        (a, b) if a != b => out.push(
            Change::new(Kind::Changed, area, path.to_string())
                .values(Some(a.to_string()), Some(b.to_string())),
        ),
        _ => {}
    }
}

/// What a step runs, without versions: the rename heuristic pairs steps
/// with equal signatures.
fn signature(s: &StepIr) -> String {
    match &s.action {
        Action::Task(i) => format!("task {}", i.task),
        Action::Workflow(i) => format!("workflow {}", i.task),
        Action::Sleep { .. } => "sleep".into(),
        Action::Await { signal, .. } => format!("await {signal}"),
        Action::Compute { .. } => "compute".into(),
    }
}

fn workflow_changes(o: &WorkflowIr, n: &WorkflowIr) -> Vec<Change> {
    let mut out = vec![];
    let field = |out: &mut Vec<Change>, subject: &str, a: Option<String>, b: Option<String>| {
        if a != b {
            out.push(
                Change::new(Kind::Changed, "workflow", subject)
                    .values(Some(opt_str(a)), Some(opt_str(b))),
            );
        }
    };
    field(
        &mut out,
        "version",
        Some(o.version.clone()),
        Some(n.version.clone()),
    );
    field(
        &mut out,
        "description",
        o.description.clone(),
        n.description.clone(),
    );
    field(
        &mut out,
        "timeout",
        o.timeout_ms.map(super::duration),
        n.timeout_ms.map(super::duration),
    );
    type_changes(&o.input, &n.input, "input", &mut out);
    type_changes(&o.output, &n.output, "output", &mut out);
    let rs = |w: &WorkflowIr| w.result.as_ref().map(pretty::expr);
    let kind = match (rs(o), rs(n)) {
        (None, Some(_)) => Kind::Added,
        (Some(_), None) => Kind::Removed,
        _ => Kind::Changed,
    };
    if rs(o) != rs(n) {
        out.push(Change::new(kind, "result", "return").values(rs(o), rs(n)));
    }

    // Signals and queries.
    for (sig, t) in &o.signals {
        match n.signals.get(sig) {
            Some(u) => type_changes(t, u, &format!("signal {sig}"), &mut out),
            None => out.push(
                Change::new(Kind::Removed, "signal", sig.clone()).values(Some(t.to_string()), None),
            ),
        }
    }
    for (sig, t) in &n.signals {
        if !o.signals.contains_key(sig) {
            out.push(
                Change::new(Kind::Added, "signal", sig.clone()).values(None, Some(t.to_string())),
            );
        }
    }
    for (q, a) in &o.queries {
        match n.queries.get(q) {
            Some(b) => {
                let (ea, eb) = (pretty::expr(&a.expr), pretty::expr(&b.expr));
                if ea != eb {
                    out.push(
                        Change::new(Kind::Changed, "query", format!("{q} expression"))
                            .values(Some(ea), Some(eb)),
                    );
                }
                type_changes(
                    &a.output_type,
                    &b.output_type,
                    &format!("query {q}"),
                    &mut out,
                );
            }
            None => out.push(
                Change::new(Kind::Removed, "query", q.clone())
                    .values(Some(a.output_type.to_string()), None),
            ),
        }
    }
    for (q, b) in &n.queries {
        if !o.queries.contains_key(q) {
            out.push(
                Change::new(Kind::Added, "query", q.clone())
                    .values(None, Some(b.output_type.to_string())),
            );
        }
    }

    // Steps, scope by scope (main, handlers, event blocks).
    let mut all_scopes: Vec<Scope> = scopes(o);
    for s in scopes(n) {
        if !all_scopes.contains(&s) {
            all_scopes.push(s);
        }
    }
    all_scopes.sort();
    for scope in all_scopes {
        let (os, ns) = (scope_steps(o, scope), scope_steps(n, scope));
        match scope {
            Scope::Main => {}
            Scope::Handler(_) | Scope::Event(_) if os.is_empty() || ns.is_empty() => {
                let area = if matches!(scope, Scope::Handler(_)) {
                    "handler"
                } else {
                    "event"
                };
                let steps = |ss: &[StepIr]| {
                    format!(
                        "{} ({})",
                        ss.len(),
                        ss.iter()
                            .map(|s| s.id.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                };
                if os.is_empty() {
                    out.push(
                        Change::new(Kind::Added, area, scope.label())
                            .values(None, Some(format!("steps {}", steps(ns)))),
                    );
                } else {
                    out.push(
                        Change::new(Kind::Removed, area, scope.label())
                            .values(Some(format!("steps {}", steps(os))), None),
                    );
                }
                continue;
            }
            _ => {}
        }
        step_changes(scope, os, ns, &mut out);
    }

    // Limits.
    let lim = |w: &WorkflowIr| {
        w.limits
            .as_ref()
            .map(super::graph::limits_summary)
            .filter(|s| !s.is_empty())
    };
    if lim(o) != lim(n) {
        out.push(
            Change::new(Kind::Changed, "limits", "limits")
                .values(Some(opt_str(lim(o))), Some(opt_str(lim(n)))),
        );
    }

    // Contracts this workflow uses at a different version (the step diff
    // already shows the task@version change; this adds type changes).
    for (name, a) in &o.contracts {
        if let Some(b) = n.contracts.get(name) {
            if name.starts_with(WORKFLOW_CONTRACT_PREFIX) && a.workflow_hash != b.workflow_hash {
                out.push(
                    Change::new(Kind::Changed, "contract", format!("{name} definition")).values(
                        Some(format!("{}@{}", a.name, a.version)),
                        Some(format!("{}@{}", b.name, b.version)),
                    ),
                );
            }
        }
    }
    out
}

fn step_changes(scope: Scope, os: &[StepIr], ns: &[StepIr], out: &mut Vec<Change>) {
    let q = |id: &str| scope.qualify(id);
    let new_ids: BTreeSet<&str> = ns.iter().map(|s| s.id.as_str()).collect();
    let old_ids: BTreeSet<&str> = os.iter().map(|s| s.id.as_str()).collect();
    let removed: Vec<&StepIr> = os
        .iter()
        .filter(|s| !new_ids.contains(s.id.as_str()))
        .collect();
    let added: Vec<&StepIr> = ns
        .iter()
        .filter(|s| !old_ids.contains(s.id.as_str()))
        .collect();
    // Rename heuristic: a unique unmatched pair with the same signature.
    let mut renames: BTreeMap<String, String> = BTreeMap::new();
    for r in &removed {
        let sig = signature(r);
        let same_old = removed.iter().filter(|x| signature(x) == sig).count();
        let cands: Vec<&&StepIr> = added.iter().filter(|a| signature(a) == sig).collect();
        if same_old == 1 && cands.len() == 1 {
            renames.insert(r.id.clone(), cands[0].id.clone());
        }
    }
    let renamed_to: BTreeSet<&str> = renames.values().map(String::as_str).collect();
    let rename = |id: &str| renames.get(id).cloned().unwrap_or_else(|| id.to_string());

    for s in &removed {
        if let Some(to) = renames.get(&s.id) {
            out.push(
                Change::new(Kind::Renamed, "step", q(&s.id))
                    .values(Some(s.id.clone()), Some(to.clone())),
            );
        } else {
            out.push(
                Change::new(Kind::Removed, "step", q(&s.id)).values(Some(action_summary(s)), None),
            );
        }
    }
    for s in &added {
        if !renamed_to.contains(s.id.as_str()) {
            out.push(
                Change::new(Kind::Added, "step", q(&s.id)).values(None, Some(action_summary(s))),
            );
        }
    }
    for o in os {
        let new_id = rename(&o.id);
        let Some(n) = ns.iter().find(|s| s.id == new_id) else {
            continue;
        };
        step_detail(&q(&n.id), o, n, &rename, out);
    }
}

fn inv_expr(s: &StepIr) -> Option<String> {
    match &s.action {
        Action::Task(i) | Action::Workflow(i) => Some(pretty::expr(&i.input)),
        Action::Compute { expr } => Some(pretty::expr(expr)),
        _ => None,
    }
}

fn step_detail(
    subject: &str,
    o: &StepIr,
    n: &StepIr,
    rename: &dyn Fn(&str) -> String,
    out: &mut Vec<Change>,
) {
    let changed = |out: &mut Vec<Change>,
                   area: &'static str,
                   a: Option<String>,
                   b: Option<String>| match (a, b) {
        (a, b) if a == b => {}
        (None, Some(b)) => out.push(Change::new(Kind::Added, area, subject).values(None, Some(b))),
        (Some(a), None) => {
            out.push(Change::new(Kind::Removed, area, subject).values(Some(a), None))
        }
        (a, b) => out.push(Change::new(Kind::Changed, area, subject).values(a, b)),
    };
    let action = |s: &StepIr| {
        let mut a = action_summary(s);
        if let Some(l) = s.invocation().and_then(|i| i.language.clone()) {
            a.push_str(&format!(" using {l}"));
        }
        a
    };
    changed(out, "action", Some(action(o)), Some(action(n)));
    changed(out, "step input", inv_expr(o), inv_expr(n));
    // Dependencies, compared under new names.
    let od: BTreeSet<String> = o.dependencies.iter().map(|d| rename(d)).collect();
    let nd: BTreeSet<String> = n.dependencies.iter().cloned().collect();
    for d in od.difference(&nd) {
        out.push(
            Change::new(Kind::Removed, "dependency", subject)
                .values(Some(format!("after {d}")), None),
        );
    }
    for d in nd.difference(&od) {
        out.push(
            Change::new(Kind::Added, "dependency", subject)
                .values(None, Some(format!("after {d}"))),
        );
    }
    let cond = |s: &StepIr| s.condition.as_ref().map(pretty::expr);
    changed(out, "condition", cond(o), cond(n));
    let fan = |s: &StepIr| {
        s.for_each.as_ref().map(|f| {
            let mut t = format!("for {} in {}", f.variable, pretty::expr(&f.items));
            if let Some(m) = f.max_parallel {
                t.push_str(&format!(", max_parallel {m}"));
            }
            t
        })
    };
    changed(out, "fan-out", fan(o), fan(n));
    let retry = |s: &StepIr| s.invocation().map(|i| opt_str(retry_summary(&i.retry)));
    changed(out, "retry", retry(o), retry(n));
    changed(out, "timeout", timeout_summary(o), timeout_summary(n));
    let key = |s: &StepIr| {
        s.invocation()
            .and_then(|i| i.idempotency_key.as_ref())
            .map(pretty::expr)
    };
    changed(out, "idempotency", key(o), key(n));
    let comp = |s: &StepIr| {
        s.compensation.as_ref().map(|c| {
            let mut t = format!(
                "{}@{} with {}",
                c.task,
                c.contract_version,
                pretty::expr(&c.input)
            );
            if let Some(r) = retry_summary(&c.retry) {
                t.push_str(&format!(", retry {r}"));
            }
            t
        })
    };
    changed(out, "compensation", comp(o), comp(n));
}

impl Diff {
    pub fn change_count(&self) -> usize {
        self.workflows
            .iter()
            .map(|w| w.changes.len())
            .sum::<usize>()
            + self.contracts.len()
    }

    pub fn has_changes(&self) -> bool {
        self.change_count() > 0
    }

    fn counts(&self) -> (usize, usize, usize, usize) {
        let all = self
            .workflows
            .iter()
            .flat_map(|w| &w.changes)
            .chain(&self.contracts);
        let mut c = (0, 0, 0, 0);
        for ch in all {
            match ch.kind {
                Kind::Added => c.0 += 1,
                Kind::Removed => c.1 += 1,
                Kind::Changed => c.2 += 1,
                Kind::Renamed => c.3 += 1,
            }
        }
        c
    }

    /// `compatible`, or `breaking: ...` with the compat hints.
    pub fn verdict(&self) -> String {
        if self.compat.errors() > 0 {
            let hints: BTreeSet<&str> = self
                .compat
                .findings
                .iter()
                .filter(|f| f.severity == compat::Severity::Error)
                .filter_map(|f| f.hint.as_deref())
                .collect();
            let mut s = format!("incompatible, {} error(s)", self.compat.errors());
            if !hints.is_empty() {
                s.push_str(&format!(
                    " ({})",
                    hints.into_iter().collect::<Vec<_>>().join("; ")
                ));
            }
            s
        } else if self.compat.warnings() > 0 {
            format!("compatible, {} warning(s)", self.compat.warnings())
        } else {
            "compatible".into()
        }
    }

    /// One line: `2 workflow(s) changed, 1 added; 7 change(s): 3 added, ...; compatibility: ...`.
    pub fn summary(&self) -> String {
        if !self.has_changes() {
            return format!("no changes; compatibility: {}", self.verdict());
        }
        let count = |s: Status| self.workflows.iter().filter(|w| w.status == s).count();
        let mut wf = vec![];
        for (s, label) in [
            (Status::Changed, "changed"),
            (Status::Added, "added"),
            (Status::Removed, "removed"),
        ] {
            let n = count(s);
            if n > 0 {
                wf.push(format!("{n} {label}"));
            }
        }
        let (a, r, c, m) = self.counts();
        let mut kinds = vec![];
        for (n, label) in [(a, "added"), (r, "removed"), (c, "changed"), (m, "renamed")] {
            if n > 0 {
                kinds.push(format!("{n} {label}"));
            }
        }
        let wf = if wf.is_empty() {
            "workflows unchanged".to_string()
        } else {
            format!("workflows: {}", wf.join(", "))
        };
        format!(
            "{wf}; {} change(s): {}; compatibility: {}",
            self.change_count(),
            kinds.join(", "),
            self.verdict()
        )
    }

    fn title(w: &WorkflowDiff) -> String {
        match (&w.old_version, &w.new_version) {
            (Some(a), Some(b)) if a == b => format!("{} {a}", w.name),
            (Some(a), Some(b)) => format!("{} {a} → {b}", w.name),
            (Some(a), None) => format!("{} {a} (removed)", w.name),
            (None, Some(b)) => format!("{} {b} (added)", w.name),
            (None, None) => w.name.clone(),
        }
    }

    pub fn text(&self, color: bool) -> String {
        let paint = |s: &str, code: &str| {
            if color {
                format!("\x1b[{code}m{s}\x1b[0m")
            } else {
                s.to_string()
            }
        };
        let mut out = String::new();
        for w in &self.workflows {
            let _ = writeln!(out, "workflow {}", Self::title(w));
            if w.changes.is_empty() {
                out.push_str("  no changes\n");
            }
            for c in &w.changes {
                let line = c.text();
                let code = match c.kind {
                    Kind::Added => "32",
                    Kind::Removed => "31",
                    Kind::Changed => "33",
                    Kind::Renamed => "36",
                };
                let _ = writeln!(out, "  {}", paint(&line, code));
            }
        }
        if !self.contracts.is_empty() {
            out.push_str("contracts\n");
            for c in &self.contracts {
                let _ = writeln!(out, "  {}", c.text());
            }
        }
        let notable: Vec<&compat::Finding> = self
            .compat
            .findings
            .iter()
            .filter(|f| {
                matches!(
                    f.severity,
                    compat::Severity::Error | compat::Severity::Warning
                )
            })
            .collect();
        if !notable.is_empty() {
            out.push_str("compatibility\n");
            for f in notable {
                for l in f.render(color).lines() {
                    let _ = writeln!(out, "  {l}");
                }
            }
        }
        let _ = writeln!(out, "summary: {}", self.summary());
        out
    }

    pub fn markdown(&self) -> String {
        let cell = |s: &str| {
            if s.is_empty() {
                String::new()
            } else {
                let s = s.replace('|', "\\|");
                if s.contains('`') {
                    format!("`` {s} ``")
                } else {
                    format!("`{s}`")
                }
            }
        };
        let mut out = String::from("## Process changes\n\n");
        let _ = writeln!(out, "**Summary:** {}\n", self.summary());
        let table = |changes: &[Change], out: &mut String| {
            out.push_str("| Change | Area | Subject | Before | After |\n|---|---|---|---|---|\n");
            for c in changes {
                let _ = writeln!(
                    out,
                    "| {} | {} | {} | {} | {} |",
                    c.kind.as_str(),
                    c.area,
                    cell(&c.subject),
                    cell(c.old.as_deref().unwrap_or("")),
                    cell(c.new.as_deref().unwrap_or(""))
                );
            }
            out.push('\n');
        };
        for w in &self.workflows {
            let _ = writeln!(out, "### Workflow `{}`\n", Self::title(w));
            if w.changes.is_empty() {
                out.push_str("No changes.\n\n");
            } else {
                table(&w.changes, &mut out);
            }
        }
        if !self.contracts.is_empty() {
            out.push_str("### Task contracts\n\n");
            table(&self.contracts, &mut out);
        }
        let _ = writeln!(out, "### Compatibility\n\n**{}**\n", self.verdict());
        let notable: Vec<&compat::Finding> = self
            .compat
            .findings
            .iter()
            .filter(|f| {
                matches!(
                    f.severity,
                    compat::Severity::Error | compat::Severity::Warning
                )
            })
            .collect();
        for f in notable {
            let _ = writeln!(out, "- {}", f.render(false).replace('\n', " "));
        }
        out.trim_end().to_string() + "\n"
    }

    pub fn to_json(&self) -> Value {
        json!({
            "summary": self.summary(),
            "changes": self.change_count(),
            "workflows": self.workflows.iter().map(|w| json!({
                "name": w.name,
                "old_version": w.old_version,
                "new_version": w.new_version,
                "status": match w.status {
                    Status::Added => "added",
                    Status::Removed => "removed",
                    Status::Changed => "changed",
                    Status::Unchanged => "unchanged",
                },
                "changes": w.changes.iter().map(Change::to_json).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
            "contracts": self.contracts.iter().map(Change::to_json).collect::<Vec<_>>(),
            "compatibility": self.compat.to_json(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile_str;

    const CONTRACTS: &str = r#"
task "pay.charge" { version "1.0.0" input { amount: Float } output { id: String } }
task "pay.refund" { version "1.0.0" input { id: String } output { ok: Bool } }
task "mail.send" { version "1.0.0" input { to: String } output { ok: Bool } }
"#;

    fn snap(wf: &str) -> Snapshot {
        let c = compile_str("w.orbit", &format!("{CONTRACTS}\n{wf}"));
        assert!(!c.has_errors(), "{}", c.render_diagnostics(false));
        Snapshot::from_compilation(&c)
    }

    fn changes(old: &str, new: &str) -> Vec<String> {
        let d = diff(&snap(old), &snap(new), None);
        d.workflows
            .iter()
            .flat_map(|w| w.changes.iter().map(Change::text))
            .chain(d.contracts.iter().map(Change::text))
            .collect()
    }

    const BASE: &str = r#"
workflow Pay {
    version "1.0.0"
    input { amount: Float, to: String }
    step charge { worker "pay.charge" with { amount: input.amount } }
    step mail { worker "mail.send" after charge with { to: input.to } }
}
"#;

    #[test]
    fn identical_is_no_change() {
        let d = diff(&snap(BASE), &snap(BASE), None);
        assert!(!d.has_changes());
        assert_eq!(d.summary(), "no changes; compatibility: compatible");
    }

    #[test]
    fn step_added_removed_and_renamed() {
        let new = BASE
            .replace("step charge", "step charge_card")
            .replace("after charge", "after charge_card")
            .replace("1.0.0\"\n    input", "1.1.0\"\n    input")
            .replace(
                "step mail",
                "step pause { sleep 1h after charge_card }\n    step mail",
            );
        let c = changes(BASE, &new);
        assert!(
            c.contains(&"~ workflow: version: 1.0.0 → 1.1.0".to_string()),
            "{c:?}"
        );
        assert!(
            c.contains(&"> step: charge: charge → charge_card".to_string()),
            "{c:?}"
        );
        assert!(c.contains(&"+ step: pause: sleep 1h".to_string()), "{c:?}");
        // The dependency on the renamed step is not reported as changed.
        assert!(
            !c.iter()
                .any(|l| l.contains("after charge_card") && l.contains("mail")),
            "{c:?}"
        );
        let removed = BASE.replace(
            "    step mail { worker \"mail.send\" after charge with { to: input.to } }\n",
            "",
        );
        let c = changes(BASE, &removed);
        assert_eq!(c, ["- step: mail: mail.send@1.0.0"]);
    }

    #[test]
    fn dependencies_conditions_retry_timeout() {
        let new = BASE.replace(
            "step mail { worker \"mail.send\" after charge with { to: input.to } }",
            "step mail { worker \"mail.send\" when input.amount > 10.0 with { to: input.to } retry { attempts: 3 } timeout 30s }",
        );
        let c = changes(BASE, &new);
        assert!(
            c.contains(&"- dependency: mail: after charge".to_string()),
            "{c:?}"
        );
        assert!(
            c.contains(&"+ condition: mail: input.amount > 10.0".to_string()),
            "{c:?}"
        );
        assert!(
            c.iter()
                .any(|l| l.starts_with("~ retry: mail: none → 3 attempts")),
            "{c:?}"
        );
        assert!(
            c.contains(&"+ timeout: mail: timeout 30s".to_string()),
            "{c:?}"
        );
    }

    #[test]
    fn compensation_action_and_input() {
        let new = BASE.replace(
            "step charge { worker \"pay.charge\" with { amount: input.amount } }",
            "step charge { worker \"pay.charge\" with { amount: input.amount * 2.0 } compensate { worker \"pay.refund\" with { id: charge.id } } }",
        );
        let c = changes(BASE, &new);
        assert!(
            c.contains(
                &"+ compensation: charge: pay.refund@1.0.0 with { id: charge.id }".to_string()
            ),
            "{c:?}"
        );
        assert!(
            c.contains(
                &"~ step input: charge: { amount: input.amount } → { amount: input.amount * 2.0 }"
                    .to_string()
            ),
            "{c:?}"
        );
        let c = changes(&new, BASE);
        assert!(
            c.iter().any(|l| l.starts_with("- compensation: charge")),
            "{c:?}"
        );
        let other = BASE.replace("worker \"mail.send\"", "worker \"mail.send\" using python");
        assert_eq!(
            changes(BASE, &other),
            ["~ action: mail: mail.send@1.0.0 → mail.send@1.0.0 using python"]
        );
    }

    #[test]
    fn handlers_events_signals_queries_types_limits() {
        let new = r#"
workflow Pay {
    version "2.0.0"
    input { amount: Int, to: String, note: String = "x" }
    output { id: String }
    limits { max_fan_out: 10 }
    signal stop { why: String }
    step charge { worker "pay.charge" with { amount: float(input.amount) } }
    step mail { worker "mail.send" after charge with { to: input.to } }
    query charged = charge.id
    on failure { step tell { worker "mail.send" with { to: input.to } } }
    on event stop { step log { compute event.why } }
    return { id: charge.id }
}
"#;
        let c = changes(BASE, new);
        for expected in [
            "~ input: input.amount: Float → Int",
            "+ input: input.note: String",
            "+ output: output.id: String",
            "+ signal: stop: { why: String }",
            "+ query: charged: String?",
            "+ handler: on failure: steps 1 (tell)",
            "+ event: on event stop: steps 1 (log)",
            "~ limits: limits: none → max_fan_out 10",
            "+ result: return: { id: charge.id }",
        ] {
            assert!(
                c.contains(&expected.to_string()),
                "missing {expected}: {c:?}"
            );
        }
        let d = diff(&snap(BASE), &snap(new), None);
        assert!(d.verdict().starts_with("compatible"), "{}", d.verdict());
        // Changed handler steps are compared step by step.
        let newer = new.replace(
            "with { to: input.to } } }",
            "with { to: input.to } timeout 5s } }",
        );
        let c = changes(new, &newer);
        assert_eq!(c, ["+ timeout: on failure: tell: timeout 5s"]);
    }

    #[test]
    fn contract_changes_and_compat_verdict() {
        let old = snap(BASE);
        let src = format!("{CONTRACTS}\n{BASE}").replace(
            "task \"mail.send\" { version \"1.0.0\" input { to: String }",
            "task \"mail.send\" { version \"1.0.0\" input { to: String, cc: String? }",
        );
        let c = compile_str("w.orbit", &src);
        assert!(!c.has_errors(), "{}", c.render_diagnostics(false));
        let d = diff(&old, &Snapshot::from_compilation(&c), None);
        let lines: Vec<String> = d.contracts.iter().map(Change::text).collect();
        assert_eq!(lines, ["+ contract: mail.send input.cc: String?"]);
        // Same version, different content: compat reports an error.
        assert!(d.verdict().starts_with("incompatible"), "{}", d.verdict());
        let md = d.markdown();
        assert!(
            md.starts_with("## Process changes\n\n**Summary:** "),
            "{md}"
        );
        assert!(
            md.contains("| added | contract | `mail.send input.cc` |  | `String?` |"),
            "{md}"
        );
        let j = d.to_json();
        assert_eq!(j["compatibility"]["compatible"], false);
    }
}
