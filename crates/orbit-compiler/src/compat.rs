//! Compatibility checks between two versions of a set of workflows and task
//! contracts (`orbit compat <old> <new>`).
//!
//! The checks work on compiled IR. Workflows and contracts are matched by
//! name; every type comparison goes through [`crate::types::assignable`], so
//! the rules are the language's own subtyping rules (records may carry extra
//! fields, `Int` widens to `Float`, `T` widens to `T?`). Two refinements are
//! layered on top, both because the runtime validates more strictly than the
//! type checker:
//!
//! * workflow inputs are validated strictly (unknown fields are rejected), so
//!   removing an input field is breaking for callers that still send it;
//! * `JSON` is treated as a top type: widening to `JSON` is compatible,
//!   narrowing from `JSON` to a concrete type is breaking.

use crate::types::assignable;
use crate::{is_semver, Compilation};
use orbit_ir::{FieldType, TaskContract, Type, WorkflowIr, WORKFLOW_CONTRACT_PREFIX};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::fmt::Write as _;

/// The workflows and contracts on one side of a comparison.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub workflows: Vec<WorkflowIr>,
    /// Contracts declared by the sources, in addition to the ones the
    /// workflows reference.
    pub contracts: BTreeMap<String, TaskContract>,
}

impl Snapshot {
    /// The workflows and declared contracts of a successful compilation.
    pub fn from_compilation(c: &Compilation) -> Snapshot {
        Snapshot {
            workflows: c.workflows.clone(),
            contracts: c.contracts.clone(),
        }
    }

    /// Compiled IR documents; their contracts come from each document.
    pub fn from_workflows(workflows: Vec<WorkflowIr>) -> Snapshot {
        Snapshot {
            workflows,
            contracts: BTreeMap::new(),
        }
    }

    /// Every user contract, keyed by name and then version. Built-in tasks and
    /// child-workflow contracts (`workflow:<Name>`) are left out: the former
    /// are defined by the runtime, the latter are covered by the workflow rules.
    fn contract_versions(&self) -> BTreeMap<String, BTreeMap<String, TaskContract>> {
        let mut out: BTreeMap<String, BTreeMap<String, TaskContract>> = BTreeMap::new();
        let all = self
            .contracts
            .values()
            .chain(self.workflows.iter().flat_map(|w| w.contracts.values()));
        for c in all {
            if c.builtin || c.name.starts_with(WORKFLOW_CONTRACT_PREFIX) {
                continue;
            }
            out.entry(c.name.clone())
                .or_default()
                .entry(c.version.clone())
                .or_insert_with(|| c.clone());
        }
        out
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Error,
    Warning,
    Ok,
    Info,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Ok => "ok",
            Severity::Info => "info",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Subject {
    Workflow,
    Contract,
}

impl Subject {
    pub fn as_str(self) -> &'static str {
        match self {
            Subject::Workflow => "workflow",
            Subject::Contract => "contract",
        }
    }
}

/// One result line of a compatibility check.
#[derive(Debug, Clone, PartialEq)]
pub struct Finding {
    pub severity: Severity,
    pub subject: Subject,
    pub name: String,
    pub old_version: Option<String>,
    pub new_version: Option<String>,
    /// Field path such as `input.customer.email`, `output.items[]` or
    /// `output.tags{}` (a map value).
    pub path: Option<String>,
    pub old_type: Option<String>,
    pub new_type: Option<String>,
    pub message: String,
    pub hint: Option<String>,
}

impl Finding {
    fn new(severity: Severity, subject: Subject, name: &str, message: impl Into<String>) -> Self {
        Finding {
            severity,
            subject,
            name: name.to_string(),
            old_version: None,
            new_version: None,
            path: None,
            old_type: None,
            new_type: None,
            message: message.into(),
            hint: None,
        }
    }

    fn versions(mut self, old: Option<&str>, new: Option<&str>) -> Self {
        self.old_version = old.map(str::to_string);
        self.new_version = new.map(str::to_string);
        self
    }

    fn hint(mut self, hint: Option<String>) -> Self {
        self.hint = hint;
        self
    }

    fn delta(mut self, d: &Delta) -> Self {
        self.path = Some(d.path.clone());
        self.old_type = d.old.as_ref().map(Type::to_string);
        self.new_type = d.new.as_ref().map(Type::to_string);
        self
    }

    fn version_label(&self) -> String {
        match (&self.old_version, &self.new_version) {
            (Some(a), Some(b)) if a == b => format!("@{a}"),
            (Some(a), Some(b)) => format!(" {a} → {b}"),
            (Some(a), None) | (None, Some(a)) => format!("@{a}"),
            (None, None) => String::new(),
        }
    }

    /// `error workflow Invoice 1.4.0 → 1.5.0: input.amount: Float → Int: ...`
    pub fn render(&self, color: bool) -> String {
        let code = match self.severity {
            Severity::Error => "31",
            Severity::Warning => "33",
            Severity::Ok => "32",
            Severity::Info => "36",
        };
        let sev = format!("{:<7}", self.severity.as_str());
        let mut out = if color {
            format!("\x1b[{code}m{sev}\x1b[0m")
        } else {
            sev
        };
        let _ = write!(
            out,
            " {} {}{}",
            self.subject.as_str(),
            self.name,
            self.version_label()
        );
        if let Some(p) = &self.path {
            let _ = write!(out, ": {p}");
        }
        match (&self.old_type, &self.new_type) {
            (Some(a), Some(b)) => {
                let _ = write!(out, ": {a} → {b}");
            }
            (Some(a), None) => {
                let _ = write!(out, ": {a} → (removed)");
            }
            (None, Some(b)) => {
                let _ = write!(out, ": (added) → {b}");
            }
            (None, None) => {}
        }
        let _ = write!(out, ": {}", self.message);
        if let Some(h) = &self.hint {
            let _ = write!(out, "; hint: {h}");
        }
        out
    }

    pub fn to_json(&self) -> Value {
        let mut m = Map::new();
        m.insert("severity".into(), json!(self.severity.as_str()));
        m.insert("kind".into(), json!(self.subject.as_str()));
        m.insert("name".into(), json!(self.name));
        let opt = |m: &mut Map<String, Value>, k: &str, v: &Option<String>| {
            if let Some(v) = v {
                m.insert(k.into(), json!(v));
            }
        };
        opt(&mut m, "old_version", &self.old_version);
        opt(&mut m, "new_version", &self.new_version);
        opt(&mut m, "path", &self.path);
        opt(&mut m, "old_type", &self.old_type);
        opt(&mut m, "new_type", &self.new_type);
        m.insert("message".into(), json!(self.message));
        opt(&mut m, "hint", &self.hint);
        Value::Object(m)
    }
}

/// The result of comparing two snapshots.
#[derive(Debug, Clone, Default)]
pub struct Report {
    pub findings: Vec<Finding>,
}

impl Report {
    pub fn count(&self, severity: Severity) -> usize {
        self.findings
            .iter()
            .filter(|f| f.severity == severity)
            .count()
    }

    pub fn errors(&self) -> usize {
        self.count(Severity::Error)
    }

    pub fn warnings(&self) -> usize {
        self.count(Severity::Warning)
    }

    pub fn render(&self, color: bool) -> String {
        self.findings
            .iter()
            .map(|f| f.render(color) + "\n")
            .collect()
    }

    pub fn to_json(&self) -> Value {
        json!({
            "compatible": self.errors() == 0,
            "errors": self.errors(),
            "warnings": self.warnings(),
            "findings": self.findings.iter().map(Finding::to_json).collect::<Vec<_>>(),
        })
    }
}

/// Compare two snapshots. Workflows are reported first, then contracts, each
/// sorted by name.
pub fn compare(old: &Snapshot, new: &Snapshot) -> Report {
    let mut r = Report::default();
    let by_name = |s: &Snapshot| -> BTreeMap<String, WorkflowIr> {
        s.workflows
            .iter()
            .map(|w| (w.name.clone(), w.clone()))
            .collect()
    };
    let (ow, nw) = (by_name(old), by_name(new));
    for (name, o) in &ow {
        match nw.get(name) {
            Some(n) => compare_workflows(o, n, &mut r),
            None => r.findings.push(
                Finding::new(
                    Severity::Warning,
                    Subject::Workflow,
                    name,
                    "workflow removed; running executions keep their definition, but new starts fail once it is no longer registered",
                )
                .versions(Some(&o.version), None),
            ),
        }
    }
    for (name, n) in &nw {
        if !ow.contains_key(name) {
            r.findings.push(
                Finding::new(Severity::Info, Subject::Workflow, name, "workflow added")
                    .versions(None, Some(&n.version)),
            );
        }
    }

    let (oc, nc) = (old.contract_versions(), new.contract_versions());
    for (name, ovs) in &oc {
        match nc.get(name) {
            Some(nvs) => compare_contract_versions(name, ovs, nvs, &mut r),
            None => {
                for v in ovs.keys() {
                    r.findings.push(
                        Finding::new(
                            Severity::Info,
                            Subject::Contract,
                            name,
                            "contract no longer declared or used",
                        )
                        .versions(Some(v), None),
                    );
                }
            }
        }
    }
    for (name, nvs) in &nc {
        if !oc.contains_key(name) {
            for v in nvs.keys() {
                r.findings.push(
                    Finding::new(Severity::Info, Subject::Contract, name, "contract added")
                        .versions(None, Some(v)),
                );
            }
        }
    }
    r
}

fn definition_hash(w: &WorkflowIr) -> String {
    if w.definition_hash.is_empty() {
        w.compute_hash()
    } else {
        w.definition_hash.clone()
    }
}

fn compare_workflows(o: &WorkflowIr, n: &WorkflowIr, r: &mut Report) {
    let name = &o.name;
    let mut deltas = vec![];
    walk(&o.input, &n.input, Flow::Accept, true, "input", &mut deltas);
    walk(
        &o.output,
        &n.output,
        Flow::Produce,
        false,
        "output",
        &mut deltas,
    );
    // Signals are inputs sent by other systems; queries are outputs read by them.
    for (sig, old_ty) in &o.signals {
        let path = format!("signal {sig}");
        match n.signals.get(sig) {
            Some(new_ty) => walk(old_ty, new_ty, Flow::Accept, true, &path, &mut deltas),
            None => push(
                &mut deltas,
                &path,
                Some(old_ty),
                None,
                true,
                "signal removed: senders get UNKNOWN_SIGNAL",
            ),
        }
    }
    for (q, old_q) in &o.queries {
        let path = format!("query {q}");
        match n.queries.get(q) {
            Some(new_q) => walk(
                &old_q.output_type,
                &new_q.output_type,
                Flow::Produce,
                false,
                &path,
                &mut deltas,
            ),
            None => push(
                &mut deltas,
                &path,
                Some(&old_q.output_type),
                None,
                true,
                "query removed: readers get UNKNOWN_QUERY",
            ),
        }
    }
    dedupe(&mut deltas);
    let breaking = deltas.iter().any(|d| d.breaking);
    let changed_hash = definition_hash(o) != definition_hash(n);
    let versions = |f: Finding| f.versions(Some(&o.version), Some(&n.version));

    if o.version == n.version {
        if changed_hash {
            let kind = if breaking {
                "major"
            } else if deltas.is_empty() {
                "patch"
            } else {
                "minor"
            };
            let hint = next_version(&o.version, kind)
                .map(|v| format!("bump the version ({} → {v})", o.version))
                .unwrap_or_else(|| "bump the version".into());
            r.findings.push(versions(
                Finding::new(
                    Severity::Error,
                    Subject::Workflow,
                    name,
                    "content changed without a version change; the runtime rejects it with DEFINITION_CONFLICT",
                )
                .hint(Some(hint)),
            ));
        } else {
            r.findings.push(versions(Finding::new(
                Severity::Ok,
                Subject::Workflow,
                name,
                "unchanged",
            )));
        }
        return;
    }

    let (ov, nv) = (semver(&o.version), semver(&n.version));
    if let (Some(a), Some(b)) = (ov, nv) {
        if b < a {
            r.findings.push(versions(Finding::new(
                Severity::Warning,
                Subject::Workflow,
                name,
                "version decreased; starts without an explicit version still pick the highest registered version",
            )));
        }
    }
    let major = match (ov, nv) {
        (Some(a), Some(b)) => Some(is_major_bump(a, b)),
        _ => None,
    };

    let mut renamed = n.clone();
    renamed.version = o.version.clone();
    if renamed.compute_hash() == o.compute_hash() {
        r.findings.push(versions(Finding::new(
            Severity::Info,
            Subject::Workflow,
            name,
            "version changed but the definition is identical",
        )));
        return;
    }
    if deltas.is_empty() {
        r.findings.push(versions(Finding::new(
            Severity::Ok,
            Subject::Workflow,
            name,
            "definition changed; input and output unchanged",
        )));
        return;
    }
    for d in &deltas {
        let who = if d.path.starts_with("input") {
            "callers"
        } else {
            "consumers of results"
        };
        let f = if !d.breaking {
            Finding::new(Severity::Ok, Subject::Workflow, name, d.reason.clone())
        } else {
            match major {
                Some(true) => Finding::new(
                    Severity::Info,
                    Subject::Workflow,
                    name,
                    format!("{}; breaking for {who}, allowed by the major version bump", d.reason),
                ),
                Some(false) => Finding::new(
                    Severity::Error,
                    Subject::Workflow,
                    name,
                    format!("{}; breaking for {who} without a major version bump", d.reason),
                )
                .hint(major_hint(&o.version)),
                None => Finding::new(
                    Severity::Warning,
                    Subject::Workflow,
                    name,
                    format!(
                        "{}; breaking for {who}, and the bump cannot be checked because a version is not MAJOR.MINOR.PATCH",
                        d.reason
                    ),
                ),
            }
        };
        r.findings.push(versions(f.delta(d)));
    }
}

fn compare_contract_versions(
    name: &str,
    old: &BTreeMap<String, TaskContract>,
    new: &BTreeMap<String, TaskContract>,
    r: &mut Report,
) {
    for (v, o) in old {
        let Some(n) = new.get(v) else { continue };
        let versions = |f: Finding| f.versions(Some(v), Some(v));
        if o.input == n.input && o.output == n.output {
            r.findings.push(versions(Finding::new(
                Severity::Ok,
                Subject::Contract,
                name,
                "unchanged",
            )));
            continue;
        }
        // Old and new workers both announce name@version, and old and new
        // workflows both use it, so every change must be safe both ways.
        let mut deltas = vec![];
        walk(
            &o.input,
            &n.input,
            Flow::Accept,
            false,
            "input",
            &mut deltas,
        );
        walk(
            &o.input,
            &n.input,
            Flow::Produce,
            false,
            "input",
            &mut deltas,
        );
        walk(
            &o.output,
            &n.output,
            Flow::Produce,
            false,
            "output",
            &mut deltas,
        );
        walk(
            &o.output,
            &n.output,
            Flow::Accept,
            false,
            "output",
            &mut deltas,
        );
        dedupe(&mut deltas);
        let hint = next_version(v, "major")
            .map(|nv| format!("give the changed contract a new version ({v} → {nv})"));
        let mut reported = false;
        for d in &deltas {
            let f = if d.breaking {
                Finding::new(
                    Severity::Error,
                    Subject::Contract,
                    name,
                    format!(
                        "{}; contract shape changed without a version change, so workers announcing {name}@{v} no longer match every workflow that uses it",
                        d.reason
                    ),
                )
                .hint(hint.clone())
            } else {
                Finding::new(
                    Severity::Ok,
                    Subject::Contract,
                    name,
                    format!(
                        "{}; compatible for old and new workers under the same version",
                        d.reason
                    ),
                )
            };
            r.findings.push(versions(f.delta(d)));
            reported = true;
        }
        if !reported {
            r.findings.push(versions(Finding::new(
                Severity::Ok,
                Subject::Contract,
                name,
                "only schema names changed",
            )));
        }
    }

    for (v, n) in new {
        if old.contains_key(v) {
            continue;
        }
        let Some(o) = previous_version(old, v) else {
            continue;
        };
        let versions = |f: Finding| f.versions(Some(&o.version), Some(v));
        // Can a worker built for the old version serve the new one? New
        // workflows send new inputs to it, and must accept its old outputs.
        let mut deltas = vec![];
        walk(
            &o.input,
            &n.input,
            Flow::Produce,
            false,
            "input",
            &mut deltas,
        );
        walk(
            &o.output,
            &n.output,
            Flow::Accept,
            false,
            "output",
            &mut deltas,
        );
        dedupe(&mut deltas);
        let major = match (semver(&o.version), semver(v)) {
            (Some(a), Some(b)) => Some(is_major_bump(a, b)),
            _ => None,
        };
        if deltas.is_empty() {
            r.findings.push(versions(Finding::new(
                Severity::Ok,
                Subject::Contract,
                name,
                "new contract version with the same input and output",
            )));
        }
        for d in &deltas {
            let f = if !d.breaking {
                Finding::new(
                    Severity::Ok,
                    Subject::Contract,
                    name,
                    format!(
                        "{}; an implementation of {} also satisfies {v}",
                        d.reason, o.version
                    ),
                )
            } else if major == Some(true) {
                Finding::new(
                    Severity::Info,
                    Subject::Contract,
                    name,
                    format!(
                        "{}; workers must implement {v} before workflows that use it are deployed",
                        d.reason
                    ),
                )
            } else {
                Finding::new(
                    Severity::Warning,
                    Subject::Contract,
                    name,
                    format!(
                        "{}; an implementation of {} cannot serve {v}, but the version bump is not major",
                        d.reason, o.version
                    ),
                )
                .hint(major_hint(&o.version))
            };
            r.findings.push(versions(f.delta(d)));
        }
    }

    for v in old.keys() {
        if !new.contains_key(v) {
            r.findings.push(
                Finding::new(
                    Severity::Info,
                    Subject::Contract,
                    name,
                    "contract version no longer declared or used; keep workers for it until executions that use it finish",
                )
                .versions(Some(v), None),
            );
        }
    }
}

/// The old version a new contract version is compared with: the highest old
/// version below it, or else the highest old version.
fn previous_version<'a>(
    old: &'a BTreeMap<String, TaskContract>,
    v: &str,
) -> Option<&'a TaskContract> {
    let target = semver(v);
    let mut versions: Vec<&TaskContract> = old.values().collect();
    versions.sort_by_key(|c| semver(&c.version));
    if let Some(t) = target {
        if let Some(c) = versions
            .iter()
            .rev()
            .find(|c| semver(&c.version).is_some_and(|s| s < t))
        {
            return Some(c);
        }
    }
    versions.last().copied()
}

/// Drop repeated deltas, and compatible deltas at a path that also has a
/// breaking one.
fn dedupe(deltas: &mut Vec<Delta>) {
    let mut seen: Vec<(String, bool)> = vec![];
    deltas.retain(|d| {
        let key = (d.path.clone(), d.breaking);
        let dup = seen.contains(&key);
        seen.push(key);
        !dup
    });
    let broken: Vec<String> = deltas
        .iter()
        .filter(|d| d.breaking)
        .map(|d| d.path.clone())
        .collect();
    deltas.retain(|d| d.breaking || !broken.contains(&d.path));
}

/// Which way values flow between the old and the new type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flow {
    /// The new type must accept values of the old type (inputs: old callers
    /// keep sending old-shaped values).
    Accept,
    /// Values of the new type must be accepted where the old type was expected
    /// (outputs: old consumers read new-shaped values).
    Produce,
}

/// One difference between an old and a new type.
#[derive(Debug, Clone, PartialEq)]
struct Delta {
    path: String,
    old: Option<Type>,
    new: Option<Type>,
    breaking: bool,
    reason: String,
}

fn push(
    out: &mut Vec<Delta>,
    path: &str,
    old: Option<&Type>,
    new: Option<&Type>,
    breaking: bool,
    reason: impl Into<String>,
) {
    out.push(Delta {
        path: path.to_string(),
        old: old.cloned(),
        new: new.cloned(),
        breaking,
        reason: reason.into(),
    });
}

/// Structural comparison of `old` and `new`. Records, lists, maps and
/// optionals are descended into so that each change is reported at its own
/// path; everything else is decided by [`assignable`].
fn walk(old: &Type, new: &Type, flow: Flow, strict: bool, path: &str, out: &mut Vec<Delta>) {
    if old == new {
        return;
    }
    let (from, to) = match flow {
        Flow::Accept => (old, new),
        Flow::Produce => (new, old),
    };
    match (old, new) {
        _ if *to == Type::Json => {
            push(out, path, Some(old), Some(new), false, "widened to JSON");
        }
        _ if *from == Type::Json => {
            push(
                out,
                path,
                Some(old),
                Some(new),
                true,
                format!("`JSON` accepts any value but `{to}` does not"),
            );
        }
        (Type::Record { fields: of, .. }, Type::Record { fields: nf, .. }) => {
            walk_fields(of, nf, flow, strict, path, out)
        }
        (Type::Optional { inner: a }, Type::Optional { inner: b }) => {
            walk(a, b, flow, strict, path, out)
        }
        (Type::List { item: a }, Type::List { item: b }) => {
            walk(a, b, flow, strict, &format!("{path}[]"), out)
        }
        (Type::Map { value: a, .. }, Type::Map { value: b, .. }) => {
            walk(a, b, flow, strict, &format!("{path}{{}}"), out)
        }
        _ => match (from, to) {
            (_, Type::Optional { inner }) => {
                // `T` flows into `T?`: compatible; keep looking inside.
                let reason = match flow {
                    Flow::Accept => "now optional",
                    Flow::Produce => "no longer null",
                };
                push(out, path, Some(old), Some(new), false, reason);
                let before = out.len();
                match flow {
                    Flow::Accept => walk(old, inner, flow, strict, path, out),
                    Flow::Produce => walk(inner, new, flow, strict, path, out),
                }
                // Changes found inside are reported with the real types.
                for d in &mut out[before..] {
                    if d.path == path {
                        d.old = Some(old.clone());
                        d.new = Some(new.clone());
                    }
                }
            }
            (Type::Optional { .. }, _) => {
                let reason = match flow {
                    Flow::Accept => format!("`{to}` is now required but old values may be null"),
                    Flow::Produce => format!("may now be null but consumers expect `{to}`"),
                };
                push(out, path, Some(old), Some(new), true, reason);
            }
            _ => match assignable(from, to) {
                Ok(()) => push(
                    out,
                    path,
                    Some(old),
                    Some(new),
                    false,
                    match flow {
                        Flow::Accept => "widened; old values are still accepted",
                        Flow::Produce => "narrowed; still accepted where the old type is expected",
                    },
                ),
                Err(e) => push(
                    out,
                    path,
                    Some(old),
                    Some(new),
                    true,
                    format!("incompatible type: {e}"),
                ),
            },
        },
    }
}

fn walk_fields(
    of: &[FieldType],
    nf: &[FieldType],
    flow: Flow,
    strict: bool,
    path: &str,
    out: &mut Vec<Delta>,
) {
    let at = |name: &str| format!("{path}.{name}");
    for o in of {
        match nf.iter().find(|n| n.name == o.name) {
            Some(n) => {
                if flow == Flow::Accept && !o.required() && n.required() && !o.ty.is_optional() {
                    push(
                        out,
                        &at(&o.name),
                        Some(&o.ty),
                        Some(&n.ty),
                        true,
                        "default removed; the field is now required",
                    );
                } else if flow == Flow::Accept && o.required() && !n.required() && !n.ty.is_optional() {
                    push(out, &at(&o.name), Some(&o.ty), Some(&n.ty), false, "default added");
                }
                walk(&o.ty, &n.ty, flow, strict, &at(&o.name), out);
            }
            None => match flow {
                Flow::Accept if strict => push(
                    out,
                    &at(&o.name),
                    Some(&o.ty),
                    None,
                    true,
                    "field removed; values that still contain it are rejected, because inputs are validated strictly",
                ),
                Flow::Accept => push(
                    out,
                    &at(&o.name),
                    Some(&o.ty),
                    None,
                    false,
                    "field removed; extra fields are ignored",
                ),
                Flow::Produce if o.required() => push(
                    out,
                    &at(&o.name),
                    Some(&o.ty),
                    None,
                    true,
                    "field removed",
                ),
                Flow::Produce => push(
                    out,
                    &at(&o.name),
                    Some(&o.ty),
                    None,
                    false,
                    "optional field removed",
                ),
            },
        }
    }
    for n in nf {
        if of.iter().any(|o| o.name == n.name) {
            continue;
        }
        match flow {
            Flow::Accept if n.required() => push(
                out,
                &at(&n.name),
                None,
                Some(&n.ty),
                true,
                "required field added without a default",
            ),
            Flow::Accept => push(
                out,
                &at(&n.name),
                None,
                Some(&n.ty),
                false,
                if n.ty.is_optional() {
                    "optional field added"
                } else {
                    "field with a default added"
                },
            ),
            Flow::Produce => push(out, &at(&n.name), None, Some(&n.ty), false, "field added"),
        }
    }
}

type Version = (u64, u64, u64);

/// `MAJOR.MINOR.PATCH`, ignoring a pre-release or build suffix.
fn semver(v: &str) -> Option<Version> {
    if !is_semver(v) {
        return None;
    }
    let core = v.split(['-', '+']).next()?;
    let mut it = core.split('.').map(|p| p.parse::<u64>().ok());
    Some((it.next()??, it.next()??, it.next()??))
}

/// A breaking change needs a new major version; in `0.x` a new minor version
/// counts as major (semver §4).
fn is_major_bump(old: Version, new: Version) -> bool {
    new.0 > old.0 || (old.0 == 0 && new.0 == 0 && new.1 > old.1)
}

fn next_version(v: &str, kind: &str) -> Option<String> {
    let (a, b, c) = semver(v)?;
    Some(match kind {
        "major" if a == 0 => format!("0.{}.0", b + 1),
        "major" => format!("{}.0.0", a + 1),
        "minor" => format!("{a}.{}.0", b + 1),
        _ => format!("{a}.{b}.{}", c + 1),
    })
}

fn major_hint(v: &str) -> Option<String> {
    let next = next_version(v, "major")?;
    Some(if v.starts_with("0.") {
        format!("bump the minor version ({v} → {next}); in 0.x versions the minor version is the major version")
    } else {
        format!("bump the major version ({v} → {next})")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semver_rules() {
        assert_eq!(semver("1.4.0-rc.1"), Some((1, 4, 0)));
        assert_eq!(semver("v1"), None);
        assert!(is_major_bump((1, 4, 0), (2, 0, 0)));
        assert!(!is_major_bump((1, 4, 0), (1, 5, 0)));
        assert!(is_major_bump((0, 3, 1), (0, 4, 0)));
        assert!(!is_major_bump((0, 3, 1), (0, 3, 2)));
        assert_eq!(next_version("1.4.2", "major").unwrap(), "2.0.0");
        assert_eq!(next_version("0.3.1", "major").unwrap(), "0.4.0");
        assert_eq!(next_version("1.4.2", "patch").unwrap(), "1.4.3");
    }
}
