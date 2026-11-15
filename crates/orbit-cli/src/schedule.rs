//! `orbit schedule ...`: manage schedules (`/v1/schedules`).

use crate::view::{paint, BOLD, CYAN, GREEN, YELLOW};
use crate::Remote;
use clap::{Subcommand, ValueEnum};
use serde_json::{json, Value};
use std::fmt::Write;
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Clone, Copy, ValueEnum)]
pub enum OverlapArg {
    /// Do not start while the previous execution of the schedule runs.
    Skip,
    /// Start anyway.
    Allow,
    /// Cancel the previous execution, then start.
    CancelPrevious,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum CatchupArg {
    /// Skip occurrences missed while no server could fire them.
    SkipMissed,
    /// Start one execution for the most recent missed occurrence.
    RunOnce,
}

#[derive(Subcommand)]
pub enum ScheduleCommand {
    /// Create a schedule that starts a workflow on a cron expression or an interval.
    Create {
        /// Schedule id: letters, digits, `-`, `_` and `.`.
        id: String,
        /// The workflow to start.
        #[arg(long)]
        workflow: String,
        /// Workflow version (default: the latest registered version at each fire).
        #[arg(long)]
        version: Option<String>,
        /// Cron expression: minute hour day-of-month month day-of-week, e.g. "0 2 * * *".
        #[arg(long, conflicts_with = "every", required_unless_present = "every")]
        cron: Option<String>,
        /// Fixed interval instead of cron, e.g. 15m or 1h30m (aligned to --start-at or the Unix epoch).
        #[arg(long)]
        every: Option<String>,
        /// IANA time zone for --cron, e.g. Europe/Berlin (default UTC).
        #[arg(long, requires = "cron")]
        timezone: Option<String>,
        /// Workflow input as JSON.
        #[arg(short, long)]
        input: Option<String>,
        /// Read the workflow input from a JSON file.
        #[arg(long, conflicts_with = "input")]
        input_file: Option<PathBuf>,
        /// What to do when an occurrence is due while the previous execution still runs.
        #[arg(long, value_enum, default_value = "skip")]
        overlap: OverlapArg,
        /// What to do with occurrences missed while no server was running.
        #[arg(long, value_enum, default_value = "skip-missed")]
        catchup: CatchupArg,
        /// No occurrence before this RFC 3339 time.
        #[arg(long)]
        start_at: Option<String>,
        /// No occurrence after this RFC 3339 time.
        #[arg(long)]
        end_at: Option<String>,
        /// Delay each fire by up to this long (deterministic per occurrence), e.g. 30s.
        #[arg(long)]
        jitter: Option<String>,
        /// Free text shown by `orbit schedule get`.
        #[arg(long)]
        description: Option<String>,
        /// Create the schedule paused.
        #[arg(long)]
        paused: bool,
        #[command(flatten)]
        remote: Remote,
    },
    /// Change a schedule's timing, input or policies (other settings are kept).
    Update {
        /// The schedule id.
        id: String,
        /// New cron expression (replaces an interval).
        #[arg(long, conflicts_with = "every")]
        cron: Option<String>,
        /// New interval (replaces a cron expression).
        #[arg(long)]
        every: Option<String>,
        /// IANA time zone for the cron expression.
        #[arg(long)]
        timezone: Option<String>,
        /// New workflow input as JSON.
        #[arg(short, long)]
        input: Option<String>,
        /// Pin a workflow version.
        #[arg(long)]
        version: Option<String>,
        /// New overlap policy.
        #[arg(long, value_enum)]
        overlap: Option<OverlapArg>,
        /// New catch-up policy.
        #[arg(long, value_enum)]
        catchup: Option<CatchupArg>,
        /// New maximum fire delay, e.g. 30s.
        #[arg(long)]
        jitter: Option<String>,
        #[command(flatten)]
        remote: Remote,
    },
    /// List schedules with their next fire time.
    List {
        #[command(flatten)]
        remote: Remote,
    },
    /// Show a schedule, its state and its next fire times.
    Get {
        /// The schedule id.
        id: String,
        /// Print the schedule as JSON.
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        remote: Remote,
    },
    /// Stop firing a schedule until it is resumed.
    Pause {
        /// The schedule id.
        id: String,
        #[command(flatten)]
        remote: Remote,
    },
    /// Resume a paused schedule from its next occurrence (missed ones are skipped).
    Resume {
        /// The schedule id.
        id: String,
        #[command(flatten)]
        remote: Remote,
    },
    /// Start an execution of the schedule now (the overlap policy applies).
    Trigger {
        /// The schedule id.
        id: String,
        #[command(flatten)]
        remote: Remote,
    },
    /// Delete a schedule (executions it started keep running).
    Delete {
        /// The schedule id.
        id: String,
        #[command(flatten)]
        remote: Remote,
    },
}

fn timing(s: &Value) -> String {
    match (s["cron"].as_str(), s["interval"].as_str()) {
        (Some(c), _) => match s["timezone"].as_str() {
            Some(tz) => format!("cron \"{c}\" {tz}"),
            None => format!("cron \"{c}\""),
        },
        (None, Some(i)) => format!("every {i}"),
        _ => String::new(),
    }
}

fn state(s: &Value) -> &'static str {
    if s["paused"].as_bool() == Some(true) {
        "paused"
    } else if s["next_fire_at"].is_null() {
        "ended"
    } else {
        "active"
    }
}

fn state_color(st: &str) -> &'static str {
    match st {
        "active" => GREEN,
        "paused" => YELLOW,
        _ => CYAN,
    }
}

fn text(v: &Value) -> &str {
    v.as_str().unwrap_or("-")
}

pub fn list(r: &Value, tty: bool) -> String {
    let mut out = format!(
        "{:<24} {:<20} {:<32} {:<8} {:<22} {}\n",
        "SCHEDULE", "WORKFLOW", "TIMING", "STATE", "NEXT FIRE", "LAST EXECUTION"
    );
    for s in r["schedules"].as_array().cloned().unwrap_or_default() {
        let st = state(&s);
        let _ = writeln!(
            out,
            "{:<24} {:<20} {:<32} {} {:<22} {}",
            text(&s["schedule_id"]),
            text(&s["workflow"]),
            timing(&s),
            paint(&format!("{st:<8}"), state_color(st), tty),
            text(&s["next_fire_at"]),
            text(&s["last_execution_id"]),
        );
    }
    out
}

pub fn details(s: &Value, tty: bool) -> String {
    let mut out = String::new();
    let st = state(s);
    let _ = writeln!(
        out,
        "{} {}  {}",
        paint(st, state_color(st), tty),
        paint(text(&s["schedule_id"]), BOLD, tty),
        timing(s)
    );
    let workflow = match s["version"].as_str() {
        Some(v) => format!("{}@{v}", text(&s["workflow"])),
        None => format!("{} (latest version)", text(&s["workflow"])),
    };
    let _ = writeln!(out, "  workflow:   {workflow}");
    let _ = writeln!(
        out,
        "  input:      {}",
        serde_json::to_string(&s["input"]).unwrap_or_default()
    );
    let _ = writeln!(
        out,
        "  overlap:    {}   catchup: {}",
        text(&s["overlap"]),
        text(&s["catchup"])
    );
    for (label, key) in [
        ("start at", "start_at"),
        ("end at", "end_at"),
        ("jitter", "jitter"),
        ("description", "description"),
    ] {
        if let Some(v) = s[key].as_str() {
            let _ = writeln!(out, "  {:<11} {v}", format!("{label}:"));
        }
    }
    let _ = writeln!(
        out,
        "  fired:      {}   skipped: {}",
        s["fired"], s["skipped"]
    );
    let _ = writeln!(out, "  last fire:  {}", text(&s["last_fire_at"]));
    let _ = writeln!(out, "  last run:   {}", text(&s["last_execution_id"]));
    if let Some(e) = s["last_error"].as_str() {
        let _ = writeln!(out, "  last error: {e}");
    }
    let times = s["next_fire_times"].as_array().cloned().unwrap_or_default();
    if times.is_empty() {
        let _ = writeln!(out, "  next fires: none");
    } else {
        let _ = writeln!(out, "  next fires:");
        for t in times {
            let _ = writeln!(out, "    {}", text(&t));
        }
    }
    out
}

fn overlap_name(o: OverlapArg) -> &'static str {
    match o {
        OverlapArg::Skip => "skip",
        OverlapArg::Allow => "allow",
        OverlapArg::CancelPrevious => "cancel_previous",
    }
}

fn catchup_name(c: CatchupArg) -> &'static str {
    match c {
        CatchupArg::SkipMissed => "skip_missed",
        CatchupArg::RunOnce => "run_once",
    }
}

fn read_input(input: Option<String>, input_file: Option<PathBuf>) -> Result<Value, String> {
    crate::parse_input(input, input_file)
}

pub fn run(cmd: ScheduleCommand, tty: bool) -> Result<ExitCode, String> {
    let err = |e: crate::client::ApiError| e.to_string();
    match cmd {
        ScheduleCommand::Create {
            id,
            workflow,
            version,
            cron,
            every,
            timezone,
            input,
            input_file,
            overlap,
            catchup,
            start_at,
            end_at,
            jitter,
            description,
            paused,
            remote,
        } => {
            let mut body = json!({
                "schedule_id": id,
                "workflow": workflow,
                "input": read_input(input, input_file)?,
                "overlap": overlap_name(overlap),
                "catchup": catchup_name(catchup),
                "paused": paused,
            });
            for (k, v) in [
                ("version", version),
                ("cron", cron),
                ("interval", every),
                ("timezone", timezone),
                ("start_at", start_at),
                ("end_at", end_at),
                ("jitter", jitter),
                ("description", description),
            ] {
                if let Some(v) = v {
                    body[k] = json!(v);
                }
            }
            let (_, s) = remote.client().post("/v1/schedules", &body).map_err(err)?;
            print!("{}", details(&s, tty));
        }
        ScheduleCommand::Update {
            id,
            cron,
            every,
            timezone,
            input,
            version,
            overlap,
            catchup,
            jitter,
            remote,
        } => {
            let mut patch = json!({});
            if let Some(c) = cron {
                patch["cron"] = json!(c);
                patch["interval"] = Value::Null;
            }
            if let Some(e) = every {
                patch["interval"] = json!(e);
                patch["cron"] = Value::Null;
                patch["timezone"] = Value::Null;
            }
            if let Some(i) = input {
                patch["input"] = serde_json::from_str(&i)
                    .map_err(|e| format!("--input is not valid JSON: {e}"))?;
            }
            for (k, v) in [
                ("timezone", timezone),
                ("version", version),
                ("jitter", jitter),
            ] {
                if let Some(v) = v {
                    patch[k] = json!(v);
                }
            }
            if let Some(o) = overlap {
                patch["overlap"] = json!(overlap_name(o));
            }
            if let Some(c) = catchup {
                patch["catchup"] = json!(catchup_name(c));
            }
            let s = remote
                .client()
                .patch(&format!("/v1/schedules/{id}"), &patch)
                .map_err(err)?;
            print!("{}", details(&s, tty));
        }
        ScheduleCommand::List { remote } => {
            let r = remote.client().get("/v1/schedules").map_err(err)?;
            print!("{}", list(&r, tty));
        }
        ScheduleCommand::Get { id, json, remote } => {
            let s = remote
                .client()
                .get(&format!("/v1/schedules/{id}"))
                .map_err(err)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&s).unwrap_or_default());
            } else {
                print!("{}", details(&s, tty));
            }
        }
        ScheduleCommand::Pause { id, remote } => {
            remote
                .client()
                .post(&format!("/v1/schedules/{id}/pause"), &json!({}))
                .map_err(err)?;
            println!("schedule {id} paused");
        }
        ScheduleCommand::Resume { id, remote } => {
            let (_, s) = remote
                .client()
                .post(&format!("/v1/schedules/{id}/resume"), &json!({}))
                .map_err(err)?;
            println!(
                "schedule {id} resumed; next fire {}",
                text(&s["next_fire_at"])
            );
        }
        ScheduleCommand::Trigger { id, remote } => {
            let (_, r) = remote
                .client()
                .post(&format!("/v1/schedules/{id}/trigger"), &json!({}))
                .map_err(err)?;
            if r["started"].as_bool() == Some(true) {
                println!(
                    "{} execution {}",
                    paint("started", CYAN, tty),
                    text(&r["execution_id"])
                );
            } else {
                println!(
                    "not started: the previous execution of {id} is still running (overlap: skip)"
                );
            }
        }
        ScheduleCommand::Delete { id, remote } => {
            remote
                .client()
                .delete(&format!("/v1/schedules/{id}"))
                .map_err(err)?;
            println!("schedule {id} deleted");
        }
    }
    Ok(ExitCode::SUCCESS)
}
