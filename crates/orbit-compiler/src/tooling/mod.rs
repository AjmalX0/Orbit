//! Tooling that reads compiled workflows as a reviewable process definition:
//! diagrams (`orbit graph`), reference documentation and JSON Schema
//! (`orbit docs`, `orbit schema`), process diffs (`orbit diff`) and
//! organisation policies (`orbit check --policy`).
//!
//! Everything here works on the compiled IR, so it describes exactly what the
//! runtime executes. Source text is consulted only for locations and `///`
//! doc comments (see [`source`]).

pub mod diff;
pub mod docs;
pub mod graph;
pub mod policy;
pub mod pretty;
pub mod schema;
pub mod source;
pub mod svg;

use orbit_ir::{StepIr, WorkflowIr};

/// Human-readable duration: `450ms`, `30s`, `2.5s`, `5m`, `3m05s`, `2h`, `2h15m`, `2d`, `1d6h`.
pub fn duration(ms: u64) -> String {
    const SEC: u64 = 1000;
    const MIN: u64 = 60 * SEC;
    const HOUR: u64 = 60 * MIN;
    const DAY: u64 = 24 * HOUR;
    if ms < SEC {
        format!("{ms}ms")
    } else if ms < MIN {
        if ms.is_multiple_of(SEC) {
            format!("{}s", ms / SEC)
        } else {
            format!("{:.1}s", ms as f64 / 1000.0)
        }
    } else if ms < HOUR {
        match (ms % MIN) / SEC {
            0 => format!("{}m", ms / MIN),
            s => format!("{}m{s:02}s", ms / MIN),
        }
    } else if ms < DAY {
        match (ms % HOUR) / MIN {
            0 => format!("{}h", ms / HOUR),
            m => format!("{}h{m:02}m", ms / HOUR),
        }
    } else {
        match (ms % DAY) / HOUR {
            0 => format!("{}d", ms / DAY),
            h => format!("{}d{h}h", ms / DAY),
        }
    }
}

/// Parse a duration written as in ORBIT source: `500ms`, `30s`, `5m`, `2h`, `7d`.
pub fn parse_duration(s: &str) -> Option<u64> {
    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit())?;
    let (n, unit) = s.split_at(split);
    let n: u64 = n.parse().ok()?;
    let mult = match unit {
        "ms" => 1,
        "s" => 1000,
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        _ => return None,
    };
    n.checked_mul(mult)
}

/// Glob match where `*` matches any run of characters (dots included) and
/// `?` matches one character. Everything else matches literally.
pub fn glob(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// True if any pattern matches.
pub fn glob_any(patterns: &[String], text: &str) -> bool {
    patterns.iter().any(|p| glob(p, text))
}

/// Where a step lives in a workflow: the main graph, an outcome handler or an
/// `on event` block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Scope<'a> {
    Main,
    Handler(&'a str),
    Event(&'a str),
}

impl Scope<'_> {
    /// `""`, `on failure`, `on event location`.
    pub fn label(&self) -> String {
        match self {
            Scope::Main => String::new(),
            Scope::Handler(k) => format!("on {k}"),
            Scope::Event(s) => format!("on event {s}"),
        }
    }

    /// The step reference as written in diagnostics: `charge`,
    /// `on failure: tell_customer`.
    pub fn qualify(&self, step: &str) -> String {
        match self {
            Scope::Main => step.to_string(),
            other => format!("{}: {step}", other.label()),
        }
    }
}

/// Every step of a workflow with its scope: main steps in execution order,
/// then handlers (failure, cancel, timeout), then event blocks by signal.
pub fn all_steps(wf: &WorkflowIr) -> Vec<(Scope<'_>, &StepIr)> {
    let mut out: Vec<(Scope, &StepIr)> = wf.steps.iter().map(|s| (Scope::Main, s)).collect();
    for kind in orbit_ir::HANDLER_KINDS {
        if let Some((k, steps)) = wf.handlers.get_key_value(*kind) {
            out.extend(steps.iter().map(|s| (Scope::Handler(k.as_str()), s)));
        }
    }
    for (sig, steps) in &wf.events {
        out.extend(steps.iter().map(|s| (Scope::Event(sig.as_str()), s)));
    }
    out
}

/// The steps of one scope.
pub fn scope_steps<'a>(wf: &'a WorkflowIr, scope: Scope<'_>) -> &'a [StepIr] {
    match scope {
        Scope::Main => &wf.steps,
        Scope::Handler(k) => wf.handlers.get(k).map(Vec::as_slice).unwrap_or(&[]),
        Scope::Event(s) => wf.events.get(s).map(Vec::as_slice).unwrap_or(&[]),
    }
}

/// Every scope of a workflow that has steps, in [`all_steps`] order.
pub fn scopes(wf: &WorkflowIr) -> Vec<Scope<'_>> {
    let mut out = vec![Scope::Main];
    for kind in orbit_ir::HANDLER_KINDS {
        if let Some((k, _)) = wf.handlers.get_key_value(*kind) {
            out.push(Scope::Handler(k.as_str()));
        }
    }
    out.extend(wf.events.keys().map(|s| Scope::Event(s.as_str())));
    out
}

/// Short description of what a step runs, e.g. `payments.charge@1.0.0`,
/// `child workflow Shipping`, `sleep 1h`, `await review`, `compute`.
pub fn action_summary(s: &StepIr) -> String {
    use orbit_ir::Action;
    match &s.action {
        Action::Task(i) => format!("{}@{}", i.task, i.contract_version),
        Action::Workflow(i) => format!(
            "child workflow {}",
            i.task
                .trim_start_matches(orbit_ir::WORKFLOW_CONTRACT_PREFIX)
        ),
        Action::Sleep { duration_ms } => format!("sleep {}", duration(*duration_ms)),
        Action::Await { signal, .. } => format!("await {signal}"),
        Action::Compute { .. } => "compute".to_string(),
    }
}

/// `3 attempts, exponential 1s..1m` or `none` for a single attempt.
pub fn retry_summary(r: &orbit_ir::RetryPolicy) -> Option<String> {
    if r.max_attempts <= 1 {
        return None;
    }
    let backoff = match r.backoff {
        orbit_ir::Backoff::Fixed => format!("fixed {}", duration(r.initial_delay_ms)),
        orbit_ir::Backoff::Exponential => format!(
            "exponential {}..{}",
            duration(r.initial_delay_ms),
            duration(r.max_delay_ms)
        ),
    };
    let mut s = format!("{} attempts, {backoff}", r.max_attempts);
    if !r.non_retryable.is_empty() {
        s.push_str(&format!(", non-retryable {}", r.non_retryable.join(", ")));
    }
    Some(s)
}

/// Timeouts of a step: `timeout 30s`, `queue 5m`, or the signal wait timeout.
pub fn timeout_summary(s: &StepIr) -> Option<String> {
    use orbit_ir::Action;
    let mut parts = vec![];
    match &s.action {
        Action::Task(i) | Action::Workflow(i) => {
            if let Some(t) = i.timeout_ms {
                parts.push(format!("timeout {}", duration(t)));
            }
            if let Some(t) = i.queue_timeout_ms {
                parts.push(format!("queue {}", duration(t)));
            }
        }
        Action::Await {
            timeout_ms: Some(t),
            ..
        } => parts.push(format!("timeout {}", duration(*t))),
        _ => {}
    }
    (!parts.is_empty()).then(|| parts.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs() {
        assert!(glob("payments.*", "payments.charge"));
        assert!(!glob("payments.*", "payment.charge"));
        assert!(glob("*", "anything.at.all"));
        assert!(glob("*.charge", "payments.charge"));
        assert!(glob("pay?ents.*", "payments.refund"));
        assert!(!glob("payments", "payments.charge"));
        assert!(glob("a*b*c", "aXXbYYc"));
        assert!(!glob("a*b*c", "aXXbYY"));
    }

    #[test]
    fn durations() {
        assert_eq!(duration(450), "450ms");
        assert_eq!(duration(30_000), "30s");
        assert_eq!(duration(2_500), "2.5s");
        assert_eq!(duration(300_000), "5m");
        assert_eq!(duration(2 * 86_400_000), "2d");
        assert_eq!(parse_duration("7d"), Some(7 * 86_400_000));
        assert_eq!(parse_duration("250ms"), Some(250));
        assert_eq!(parse_duration("7 days"), None);
        assert_eq!(parse_duration("d"), None);
    }
}
