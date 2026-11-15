//! `orbit replay`: check a recorded history against the current decider, and
//! reconstruct an execution's state at any point of its history.

use crate::view::{self, paint};
use crate::Remote;
use orbit_ir::WorkflowIr;
use orbit_runtime::model::{Event, HistoryEvent};
use orbit_runtime::replay::{replay, ReplayReport};
use serde_json::Value;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

pub struct ReplayArgs {
    pub execution_id: Option<String>,
    pub history: Option<PathBuf>,
    pub definition: Option<PathBuf>,
    pub workflow: Option<String>,
    pub at: Option<u64>,
    pub json: bool,
    pub remote: Remote,
}

fn read_json(path: &Path) -> Result<Value, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("{} is not valid JSON: {e}", path.display()))
}

/// Accepts `{"execution_id": ..., "events": [...]}` (the HTTP API and
/// `orbit logs --json` lines collected into an array) or a bare array.
fn parse_history(v: Value, source: &str) -> Result<(Option<String>, Vec<HistoryEvent>), String> {
    let (id, events) = match v {
        Value::Array(a) => (None, Value::Array(a)),
        Value::Object(mut m) => (
            m.get("execution_id")
                .and_then(Value::as_str)
                .map(str::to_string),
            m.remove("events")
                .ok_or_else(|| format!("{source}: expected an `events` array"))?,
        ),
        _ => return Err(format!("{source}: expected a history object or array")),
    };
    let events: Vec<HistoryEvent> = serde_json::from_value(events)
        .map_err(|e| format!("{source}: not an ORBIT event history: {e}"))?;
    if events.is_empty() {
        return Err(format!("{source}: the history has no events"));
    }
    Ok((id, events))
}

fn created(history: &[HistoryEvent]) -> Option<(&str, &str)> {
    history.iter().find_map(|h| match &h.event {
        Event::WorkflowCreated {
            workflow, version, ..
        } => Some((workflow.as_str(), version.as_str())),
        _ => None,
    })
}

/// A definition from a compiled IR file, or compiled from `.orbit` sources
/// (choosing the workflow the history ran).
fn load_definition(
    path: &Path,
    workflow: Option<&str>,
    history: &[HistoryEvent],
) -> Result<WorkflowIr, String> {
    let wfs = crate::workflows_from(path)?;
    let name = workflow.or(created(history).map(|(w, _)| w));
    if wfs.len() > 1 {
        if let Some(n) = name {
            if let Some(w) = wfs.iter().find(|w| w.name == n) {
                return Ok(w.clone());
            }
        }
    }
    crate::pick(&wfs, workflow).cloned()
}

fn event_line(e: &Event) -> String {
    serde_json::to_string(&view::redact(serde_json::to_value(e).unwrap_or_default()))
        .unwrap_or_default()
}

pub fn run(args: ReplayArgs, tty: bool) -> Result<ExitCode, String> {
    let (id, history, remote_def) = match (&args.history, &args.execution_id) {
        (Some(path), None) => {
            let (id, h) = parse_history(read_json(path)?, &path.display().to_string())?;
            (id.unwrap_or_else(|| "replay".into()), h, None)
        }
        (None, Some(id)) => {
            let client = args.remote.client();
            let v = client
                .get(&format!("/v1/executions/{id}/history"))
                .map_err(|e| e.to_string())?;
            let (_, h) = parse_history(v, "the runtime")?;
            let def = match &args.definition {
                Some(_) => None,
                None => {
                    let (w, ver) = created(&h)
                        .ok_or("the history has no WorkflowCreated event")?;
                    let d = client
                        .get(&format!("/v1/workflows/{w}/{ver}"))
                        .map_err(|e| e.to_string())?;
                    Some(
                        serde_json::from_value::<WorkflowIr>(d)
                            .map_err(|e| format!("the runtime returned an invalid definition: {e}"))?,
                    )
                }
            };
            (id.clone(), h, def)
        }
        (Some(_), Some(_)) => {
            return Err("give either an execution id or --history, not both".into())
        }
        (None, None) => {
            return Err(
                "give an execution id (replayed from the runtime) or --history <file> --definition <file>"
                    .into(),
            )
        }
    };
    let def = match (remote_def, &args.definition) {
        (Some(d), _) => d,
        (None, Some(path)) => load_definition(path, args.workflow.as_deref(), &history)?,
        (None, None) => {
            return Err(
                "--history needs --definition (a compiled IR .json or the .orbit source)".into(),
            )
        }
    };
    if let Some(at) = args.at {
        if !history.iter().any(|h| h.seq == at) {
            return Err(format!(
                "--at {at}: the history has events #{} to #{}",
                history.first().map(|h| h.seq).unwrap_or(0),
                history.last().map(|h| h.seq).unwrap_or(0)
            ));
        }
    }
    let report = replay(&id, &history, &def, args.at);
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report).unwrap());
    } else {
        print!("{}", render(&report, &def, &history, args.at, tty));
    }
    Ok(if report.consistent() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

fn render(
    r: &ReplayReport,
    def: &WorkflowIr,
    history: &[HistoryEvent],
    at: Option<u64>,
    tty: bool,
) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "replayed {} event{} of {} against {}@{} with the current decider",
        r.events,
        if r.events == 1 { "" } else { "s" },
        r.execution_id,
        def.name,
        def.version
    );
    let _ = writeln!(
        out,
        "  {} decided, {} external{}",
        r.decided,
        r.external,
        if r.divergences.is_empty() {
            String::new()
        } else {
            format!(", {} divergent", r.divergences.len())
        }
    );
    if let Some((recorded, current)) = &r.definition_mismatch {
        let _ = writeln!(
            out,
            "  {} the history was recorded with definition {recorded}, not {current}",
            paint("note:", view::YELLOW, tty)
        );
    }
    for d in &r.divergences {
        let _ = writeln!(
            out,
            "\n{} at #{}: the runtime recorded",
            paint("divergence", view::RED, tty),
            d.seq
        );
        let _ = writeln!(out, "    {}", event_line(&d.recorded));
        if d.decider.is_empty() {
            let _ = writeln!(
                out,
                "  but the current decider would do nothing at this point"
            );
        } else {
            let _ = writeln!(out, "  but the current decider would record");
            for e in &d.decider {
                let _ = writeln!(out, "    {}", event_line(e));
            }
        }
    }
    if let Some(seq) = at {
        let event = history.iter().find(|h| h.seq == seq);
        let start = history.first().map(|h| h.timestamp_ms).unwrap_or(0);
        let _ = writeln!(
            out,
            "\nstate after #{seq} {} (+{}ms):\n",
            event.map(|h| h.event.name()).unwrap_or(""),
            event.map(|h| h.timestamp_ms - start).unwrap_or(0)
        );
        let state = serde_json::to_value(&r.state).unwrap_or_default();
        out.push_str(&view::status(&state, tty));
        let outputs: Vec<(&String, &Value)> = r
            .state
            .units
            .iter()
            .filter_map(|(k, u)| u.output.as_ref().map(|o| (k, o)))
            .collect();
        if !outputs.is_empty() {
            let _ = writeln!(out, "\n  step outputs:");
            for (k, o) in outputs {
                let _ = writeln!(
                    out,
                    "    {k}: {}",
                    serde_json::to_string(&view::redact(o.clone())).unwrap_or_default()
                );
            }
        }
        if !r.state.signals.is_empty() {
            let _ = writeln!(out, "\n  signals:");
            for s in &r.state.signals {
                let _ = writeln!(
                    out,
                    "    #{} {} {}{}",
                    s.seq,
                    s.name,
                    serde_json::to_string(&view::redact(s.payload.clone())).unwrap_or_default(),
                    s.delivered_to
                        .as_ref()
                        .map(|d| format!(" -> {d}"))
                        .unwrap_or_else(|| " (buffered)".into())
                );
            }
        }
        if !r.pending.is_empty() {
            let _ = writeln!(out, "\n  next decisions:");
            for e in &r.pending {
                let _ = writeln!(out, "    {}", event_line(e));
            }
        }
    } else if r.divergences.is_empty() {
        let _ = writeln!(
            out,
            "{} every recorded decision is reproduced",
            paint("consistent:", view::GREEN, tty)
        );
    }
    out
}
