//! Human-readable rendering of IR, executions, histories and workers.

use crate::client::Client;
use orbit_ir::{Action, WorkflowIr};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::process::ExitCode;

pub const RED: &str = "31";
pub const GREEN: &str = "32";
pub const YELLOW: &str = "33";
pub const BLUE: &str = "34";
pub const CYAN: &str = "36";
pub const DIM: &str = "2";
pub const BOLD: &str = "1";

pub fn paint(s: &str, code: &str, on: bool) -> String {
    if on {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

const SENSITIVE: &[&str] = &[
    "password",
    "passwd",
    "secret",
    "token",
    "api_key",
    "apikey",
    "authorization",
    "credential",
    "private_key",
    "card_number",
    "cvv",
];

/// Replace values of sensitive-looking keys with `"***"` (RFC-0001 §13).
pub fn redact(v: Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(
            m.into_iter()
                .map(|(k, v)| {
                    let lower = k.to_lowercase();
                    // The protocol's own dispatch token is an opaque id, not a secret.
                    if k != "token" && SENSITIVE.iter().any(|s| lower.contains(s)) {
                        (k, Value::String("***".into()))
                    } else {
                        (k, redact(v))
                    }
                })
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.into_iter().map(redact).collect()),
        other => other,
    }
}

fn status_color(s: &str) -> &'static str {
    match s {
        "COMPLETED" | "SUCCEEDED" => GREEN,
        "FAILED" | "TIMED_OUT" => RED,
        "COMPENSATING" | "CANCELLED" | "RETRY_WAIT" | "SKIPPED" | "UNCERTAIN" => YELLOW,
        "RUNNING" | "DISPATCHED" => CYAN,
        _ => DIM,
    }
}

fn duration(ms: i64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        format!("{}m{:02}s", ms / 60_000, (ms % 60_000) / 1000)
    }
}

pub fn mermaid(wf: &WorkflowIr) -> String {
    let mut out = format!("flowchart TD\n    %% {}@{}\n", wf.name, wf.version);
    for s in &wf.steps {
        let label = match &s.action {
            Action::Task(i) => format!(
                "{}<br/><small>{}{}</small>",
                s.id,
                i.task,
                i.language
                    .as_ref()
                    .map(|l| format!(" · {l}"))
                    .unwrap_or_default()
            ),
            Action::Sleep { duration_ms } => format!(
                "{}<br/><small>sleep {}</small>",
                s.id,
                duration(*duration_ms as i64)
            ),
        };
        let shape = if s.condition.is_some() {
            format!("{{\"{label}\"}}")
        } else {
            format!("[\"{label}\"]")
        };
        let _ = writeln!(out, "    {}{shape}", s.id);
        for d in &s.dependencies {
            let _ = writeln!(out, "    {d} --> {}", s.id);
        }
        if let Some(c) = &s.compensation {
            let _ = writeln!(
                out,
                "    {id} -. compensate .-> {id}_undo([\"{}\"])",
                c.task,
                id = s.id
            );
        }
    }
    out
}

pub fn dot(wf: &WorkflowIr) -> String {
    let mut out = format!(
        "digraph \"{}\" {{\n  rankdir=TB;\n  node [shape=box, style=rounded];\n",
        wf.name
    );
    for s in &wf.steps {
        let label = match &s.action {
            Action::Task(i) => format!("{}\\n{}", s.id, i.task),
            Action::Sleep { duration_ms } => {
                format!("{}\\nsleep {}", s.id, duration(*duration_ms as i64))
            }
        };
        let shape = if s.condition.is_some() {
            ", shape=diamond"
        } else {
            ""
        };
        let _ = writeln!(out, "  \"{}\" [label=\"{label}\"{shape}];", s.id);
        for d in &s.dependencies {
            let _ = writeln!(out, "  \"{d}\" -> \"{}\";", s.id);
        }
        if let Some(c) = &s.compensation {
            let _ = writeln!(out, "  \"{}_undo\" [label=\"{}\", style=dashed];\n  \"{}\" -> \"{}_undo\" [style=dashed, label=\"compensate\"];", s.id, c.task, s.id, s.id);
        }
    }
    out.push_str("}\n");
    out
}

pub fn ir_summary(wf: &WorkflowIr, tty: bool) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{} {}@{}",
        paint("workflow", BOLD, tty),
        wf.name,
        wf.version
    );
    if let Some(d) = &wf.description {
        let _ = writeln!(out, "  {}", paint(d, DIM, tty));
    }
    let _ = writeln!(out, "  ir:      {}", wf.ir_version);
    let _ = writeln!(out, "  hash:    {}", wf.definition_hash);
    let _ = writeln!(out, "  input:   {}", wf.input);
    let _ = writeln!(out, "  output:  {}", wf.output);
    if let Some(t) = wf.timeout_ms {
        let _ = writeln!(out, "  timeout: {}", duration(t as i64));
    }
    let _ = writeln!(out, "\n{}", paint("steps (execution order)", BOLD, tty));
    for s in &wf.steps {
        let what = match &s.action {
            Action::Task(i) => {
                let mut extra = vec![];
                if let Some(l) = &i.language {
                    extra.push(format!("using {l}"));
                }
                if i.retry.max_attempts > 1 {
                    extra.push(
                        format!("retry {}x {:?}", i.retry.max_attempts, i.retry.backoff)
                            .to_lowercase(),
                    );
                }
                if let Some(t) = i.timeout_ms {
                    extra.push(format!("timeout {}", duration(t as i64)));
                }
                format!(
                    "{}@{} {}",
                    i.task,
                    i.contract_version,
                    paint(&extra.join(", "), DIM, tty)
                )
            }
            Action::Sleep { duration_ms } => format!("sleep {}", duration(*duration_ms as i64)),
        };
        let deps = if s.dependencies.is_empty() {
            String::new()
        } else {
            format!(" after {}", s.dependencies.join(", "))
        };
        let cond = if s.condition.is_some() {
            paint(" [conditional]", YELLOW, tty)
        } else {
            String::new()
        };
        let _ = writeln!(
            out,
            "  {:<22} {what}{}{cond}",
            s.id,
            paint(&deps, CYAN, tty)
        );
        if let Some(c) = &s.compensation {
            let _ = writeln!(
                out,
                "  {:<22} {} {}",
                "",
                paint("↺ compensate", YELLOW, tty),
                c.task
            );
        }
    }
    let _ = writeln!(out, "\n{}", paint("contracts", BOLD, tty));
    for c in wf.contracts.values() {
        let _ = writeln!(
            out,
            "  {}@{}{}\n    in:  {}\n    out: {}",
            c.name,
            c.version,
            if c.builtin { " (built-in)" } else { "" },
            c.input,
            c.output
        );
    }
    out
}

fn unit_rows(units: &serde_json::Map<String, Value>) -> Vec<(String, String, u64, String, String)> {
    units
        .iter()
        .map(|(k, u)| {
            let status = u["status"].as_str().unwrap_or("PENDING").to_string();
            let dur = match (
                u["dispatched_at_ms"]
                    .as_i64()
                    .or(u["scheduled_at_ms"].as_i64()),
                u["finished_at_ms"].as_i64(),
            ) {
                (Some(a), Some(b)) => duration(b - a),
                _ => String::new(),
            };
            let detail = u["error"]["message"]
                .as_str()
                .or(u["last_error"]["message"]
                    .as_str()
                    .filter(|_| status != "SUCCEEDED"))
                .map(str::to_string)
                .or_else(|| u["worker_id"].as_str().map(|w| format!("worker {w}")))
                .unwrap_or_default();
            (
                k.clone(),
                status,
                u["attempt"].as_u64().unwrap_or(0),
                dur,
                detail,
            )
        })
        .collect()
}

pub fn status(s: &Value, tty: bool) -> String {
    let mut out = String::new();
    let st = s["status"].as_str().unwrap_or("?");
    let _ = writeln!(
        out,
        "{} {}  {}@{}",
        paint(st, status_color(st), tty),
        s["id"].as_str().unwrap_or(""),
        s["workflow"].as_str().unwrap_or(""),
        s["version"].as_str().unwrap_or("")
    );
    if let (Some(a), Some(b)) = (
        s["created_at_ms"].as_i64(),
        s["finished_at_ms"].as_i64().or(s["updated_at_ms"].as_i64()),
    ) {
        let _ = writeln!(out, "  elapsed: {}", duration(b - a));
    }
    if let Some(c) = s["compensation"].as_str() {
        let _ = writeln!(out, "  compensation: {c}");
    }
    if let Some(e) = s["error"].as_object() {
        let _ = writeln!(
            out,
            "  error: {} {}",
            paint(e["code"].as_str().unwrap_or(""), RED, tty),
            e["message"].as_str().unwrap_or("")
        );
    }
    let _ = writeln!(
        out,
        "\n  {:<28} {:<12} {:>7} {:>8}  DETAIL",
        "STEP", "STATUS", "ATTEMPT", "TIME"
    );
    if let Some(units) = s["units"].as_object() {
        for (k, st, attempt, dur, detail) in unit_rows(units) {
            let _ = writeln!(
                out,
                "  {:<28} {} {:>7} {:>8}  {}",
                k,
                paint(&format!("{st:<12}"), status_color(&st), tty),
                attempt,
                dur,
                paint(&detail, DIM, tty)
            );
        }
    }
    if let Some(o) = s.get("output").filter(|o| !o.is_null()) {
        let _ = writeln!(
            out,
            "\n  output: {}",
            serde_json::to_string(&redact(o.clone())).unwrap()
        );
    }
    out
}

pub fn timeline(h: &Value, tty: bool) -> String {
    let mut out = String::new();
    let events = h["events"].as_array().cloned().unwrap_or_default();
    let start = events
        .first()
        .and_then(|e| e["timestamp_ms"].as_i64())
        .unwrap_or(0);
    for e in events {
        let ty = e["type"].as_str().unwrap_or("?");
        let t = e["timestamp_ms"].as_i64().unwrap_or(0) - start;
        let color = match ty {
            t if t.contains("Failed") || t.contains("TimedOut") => RED,
            t if t.contains("Succeeded")
                || t == "WorkflowCompleted"
                || t == "CompensationCompleted" =>
            {
                GREEN
            }
            t if t.contains("Retry")
                || t.contains("Compensation")
                || t.contains("Uncertain")
                || t.contains("Cancel")
                || t.contains("Stale") =>
            {
                YELLOW
            }
            _ => BLUE,
        };
        let mut detail = vec![];
        for key in [
            "step",
            "attempt",
            "worker_id",
            "reason",
            "delay_ms",
            "redelivery",
            "outcome",
        ] {
            if let Some(v) = e.get(key) {
                if !v.is_null() && v != &Value::Bool(false) {
                    detail.push(format!(
                        "{key}={}",
                        v.as_str()
                            .map(str::to_string)
                            .unwrap_or_else(|| v.to_string())
                    ));
                }
            }
        }
        if let Some(err) = e.get("error") {
            detail.push(format!(
                "error={} {}",
                err["code"].as_str().unwrap_or(""),
                err["message"].as_str().unwrap_or("")
            ));
        }
        let _ = writeln!(
            out,
            "{} {:>4}  {}  {}",
            paint(&format!("+{:>8}", duration(t)), DIM, tty),
            format!("#{}", e["seq"]),
            paint(&format!("{ty:<24}"), color, tty),
            detail.join(" ")
        );
    }
    out
}

pub fn executions(r: &Value, tty: bool) -> String {
    let mut out = format!(
        "{:<38} {:<24} {:<10} {:<13} {}\n",
        "EXECUTION", "WORKFLOW", "VERSION", "STATUS", "AGE"
    );
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    for e in r["executions"].as_array().cloned().unwrap_or_default() {
        let st = e["status"].as_str().unwrap_or("");
        let _ = writeln!(
            out,
            "{:<38} {:<24} {:<10} {} {}",
            e["execution_id"].as_str().unwrap_or(""),
            e["workflow"].as_str().unwrap_or(""),
            e["version"].as_str().unwrap_or(""),
            paint(&format!("{st:<13}"), status_color(st), tty),
            duration(now - e["created_at_ms"].as_i64().unwrap_or(now)),
        );
    }
    out
}

pub fn workers(r: &Value, tty: bool) -> String {
    let mut out = format!(
        "{:<28} {:<12} {:<8} {:>8}  {}\n",
        "WORKER", "RUNTIME", "STATE", "INFLIGHT", "TASKS"
    );
    for w in r["workers"].as_array().cloned().unwrap_or_default() {
        let online = w["online"].as_bool().unwrap_or(false);
        let tasks: Vec<String> = w["contracts"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|c| {
                format!(
                    "{}@{}",
                    c["task_name"].as_str().unwrap_or(""),
                    c["contract_version"].as_str().unwrap_or("")
                )
            })
            .collect();
        let _ = writeln!(
            out,
            "{:<28} {:<12} {} {:>8}  {}",
            w["worker_id"].as_str().unwrap_or(""),
            w["runtime"].as_str().unwrap_or(""),
            paint(
                &format!("{:<8}", if online { "online" } else { "offline" }),
                if online { GREEN } else { DIM },
                tty
            ),
            w["inflight"],
            tasks.join(", ")
        );
    }
    out
}

/// Stream step transitions until the execution finishes.
pub fn follow(client: &Client, id: &str, tty: bool) -> Result<ExitCode, String> {
    let mut seen: BTreeMap<String, (String, u64)> = BTreeMap::new();
    loop {
        let s = client
            .get(&format!("/v1/executions/{id}/wait?timeout_ms=1000"))
            .map_err(|e| e.to_string())?;
        if let Some(units) = s["units"].as_object() {
            for (k, st, attempt, dur, detail) in unit_rows(units) {
                let prev = seen.get(&k);
                if prev.map(|p| (&p.0, p.1)) != Some((&st, attempt)) {
                    let icon = match st.as_str() {
                        "SUCCEEDED" => "✓",
                        "FAILED" => "✗",
                        "SKIPPED" => "↷",
                        "RETRY_WAIT" => "↻",
                        "CANCELLED" => "⊘",
                        "DISPATCHED" => "▶",
                        _ => "·",
                    };
                    let extra = match st.as_str() {
                        "SUCCEEDED" => dur.to_string(),
                        "DISPATCHED" => format!("attempt {attempt}, {detail}"),
                        "RETRY_WAIT" | "FAILED" => detail.clone(),
                        _ => String::new(),
                    };
                    eprintln!(
                        "  {} {:<30} {} {}",
                        paint(icon, status_color(&st), tty),
                        k,
                        paint(&st.to_lowercase(), status_color(&st), tty),
                        paint(&extra, DIM, tty)
                    );
                    seen.insert(k, (st, attempt));
                }
            }
        }
        let st = s["status"].as_str().unwrap_or("");
        if matches!(st, "COMPLETED" | "FAILED" | "CANCELLED" | "TIMED_OUT") {
            eprintln!();
            eprint!(
                "{}",
                status(&s, tty)
                    .lines()
                    .take(4)
                    .filter(|l| !l.trim().is_empty() && !l.contains("STEP"))
                    .map(|l| format!("{l}\n"))
                    .collect::<String>()
            );
            if let Some(o) = s.get("output").filter(|o| !o.is_null()) {
                println!("{}", serde_json::to_string_pretty(o).unwrap());
            }
            return Ok(if st == "COMPLETED" {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            });
        }
    }
}
