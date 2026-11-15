//! `orbit test` and `orbit simulate`: run workflows in-process, without a
//! server or workers, on the runtime's simulator.

use crate::view::{self, paint};
use crate::DiagFormat;
use orbit_compiler::compile_files;
use orbit_ir::WorkflowIr;
use orbit_runtime::model::HistoryEvent;
use orbit_runtime::testing::{run_test, simulate, SampleHandler, Simulation, TestResult};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

fn timeline(history: &[HistoryEvent], tty: bool) -> String {
    view::timeline(&json!({ "events": history }), tty)
}

fn indent(text: &str, by: &str) -> String {
    text.lines().map(|l| format!("{by}{l}\n")).collect()
}

/// Simulated durations: `0ms`, `1.5s`, `2d 3h`.
fn simulated(ms: u64) -> String {
    match ms {
        0..=999 => format!("{ms}ms"),
        1000..=59_999 => format!("{:.1}s", ms as f64 / 1000.0),
        60_000..=3_599_999 => format!("{}m {}s", ms / 60_000, ms % 60_000 / 1000),
        3_600_000..=86_399_999 => format!("{}h {}m", ms / 3_600_000, ms % 3_600_000 / 60_000),
        _ => format!("{}d {}h", ms / 86_400_000, ms % 86_400_000 / 3_600_000),
    }
}

pub fn test(
    files: &[PathBuf],
    format: DiagFormat,
    filter: Option<&str>,
    tty: bool,
) -> Result<ExitCode, String> {
    if files.is_empty() {
        return Err(
            "no input files (expected one or more .orbit files with `test` declarations)".into(),
        );
    }
    let c = compile_files(files);
    if c.has_errors() {
        if format == DiagFormat::Json {
            let diags: Vec<_> = c
                .diagnostics
                .iter()
                .map(|d| d.to_json(&c.sources))
                .collect();
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({ "diagnostics": diags, "tests": [] }))
                    .unwrap()
            );
            return Ok(ExitCode::from(1));
        }
        eprintln!("{}", c.render_diagnostics(crate::color_err()));
        return Err(crate::summarize_errors(&c));
    }
    if format == DiagFormat::Human && !c.diagnostics.is_empty() {
        eprintln!("{}", c.render_diagnostics(crate::color_err()));
    }
    let (selected, filtered_out): (Vec<_>, Vec<_>) = c
        .tests
        .iter()
        .partition(|t| filter.is_none_or(|f| t.name.contains(f)));
    let human = format == DiagFormat::Human;
    if human {
        if c.tests.is_empty() {
            eprintln!("no tests found (declare them with `test \"name\" {{ ... }}`)");
        } else {
            println!(
                "running {} test{}",
                selected.len(),
                if selected.len() == 1 { "" } else { "s" }
            );
        }
    }
    let started = std::time::Instant::now();
    let mut results: Vec<TestResult> = vec![];
    for t in &selected {
        let r = run_test(&c.workflows, t);
        if human {
            let time = r
                .simulation
                .as_ref()
                .map(|s| format!(" ({} simulated)", simulated(s.elapsed_ms)))
                .unwrap_or_default();
            if r.passed {
                println!(
                    "{}      {}{}",
                    paint("ok", view::GREEN, tty),
                    r.name,
                    paint(&time, view::DIM, tty)
                );
            } else {
                println!(
                    "{}  {}{}",
                    paint("FAILED", view::RED, tty),
                    r.name,
                    paint(&time, view::DIM, tty)
                );
            }
        }
        results.push(r);
    }
    let failed: Vec<&TestResult> = results.iter().filter(|r| !r.passed).collect();
    let passed = results.len() - failed.len();
    if human {
        if !failed.is_empty() {
            println!("\nfailures:");
        }
        for r in &failed {
            let mut out = String::new();
            let _ = writeln!(
                out,
                "\n{} {} ({}:{}, {}@{})",
                paint("----", view::DIM, tty),
                paint(&r.name, view::BOLD, tty),
                r.location.file,
                r.location.line,
                r.workflow,
                r.version
            );
            for f in &r.failures {
                let _ = writeln!(out, "  {} {f}", paint("✗", view::RED, tty));
            }
            if let Some(sim) = &r.simulation {
                let _ = writeln!(out, "\n  timeline of execution `{}`:", sim.root.state.id);
                out.push_str(&indent(&timeline(&sim.root.history, tty), "    "));
                for child in &sim.children {
                    let _ = writeln!(
                        out,
                        "\n  timeline of child execution `{}` ({}):",
                        child.state.id, child.state.workflow
                    );
                    out.push_str(&indent(&timeline(&child.history, tty), "    "));
                }
            }
            print!("{out}");
        }
        let verdict = if failed.is_empty() {
            paint("ok", view::GREEN, tty)
        } else {
            paint("FAILED", view::RED, tty)
        };
        println!(
            "\ntest result: {verdict}. {passed} passed, {} failed, {} filtered out ({:.2}s)",
            failed.len(),
            filtered_out.len(),
            started.elapsed().as_secs_f64()
        );
    } else {
        let tests: Vec<Value> = results
            .iter()
            .map(|r| {
                let mut v = json!({
                    "name": r.name,
                    "workflow": r.workflow,
                    "version": r.version,
                    "file": r.location.file,
                    "line": r.location.line,
                    "status": if r.passed { "passed" } else { "failed" },
                    "failures": r.failures,
                });
                if let Some(sim) = &r.simulation {
                    let s = &sim.root.state;
                    v["execution"] = json!({
                        "status": s.status.as_str(),
                        "output": s.output,
                        "error": s.error,
                        "simulated_ms": sim.elapsed_ms,
                    });
                    if !r.passed {
                        v["history"] = json!(sim.root.history);
                    }
                }
                v
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "tests": tests,
                "passed": passed,
                "failed": failed.len(),
                "filtered_out": filtered_out.len(),
            }))
            .unwrap()
        );
    }
    Ok(if failed.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

pub struct SimulateArgs<'a> {
    pub file: &'a Path,
    pub workflow: Option<&'a str>,
    pub input: Value,
    pub mocks: Option<&'a Path>,
    pub fail: &'a [String],
    pub signal: &'a [String],
    pub json: bool,
}

fn parse_signal(s: &str) -> Result<(String, Value), String> {
    let (name, data) = match s.split_once('=') {
        Some((n, d)) => (n.trim(), d),
        None => (s.trim(), "{}"),
    };
    if name.is_empty() {
        return Err(format!("--signal `{s}`: expected name='<json>'"));
    }
    let text = match data.strip_prefix('@') {
        Some(path) => {
            std::fs::read_to_string(path).map_err(|e| format!("cannot read {path}: {e}"))?
        }
        None => data.to_string(),
    };
    let payload = serde_json::from_str(&text)
        .map_err(|e| format!("--signal {name}: payload is not valid JSON: {e}"))?;
    Ok((name.to_string(), payload))
}

pub fn simulate_cmd(args: SimulateArgs, tty: bool) -> Result<ExitCode, String> {
    let wfs: Vec<WorkflowIr> = crate::workflows_from(args.file)?;
    let wf = crate::pick(&wfs, args.workflow)?.clone();
    let mut handler = SampleHandler::default();
    if let Some(path) = args.mocks {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let mocks: BTreeMap<String, Value> = serde_json::from_str(&text).map_err(|e| {
            format!(
                "{}: expected a JSON object mapping task names to results: {e}",
                path.display()
            )
        })?;
        handler.results = mocks;
    }
    for f in args.fail {
        let (task, code) = f
            .split_once('=')
            .ok_or_else(|| format!("--fail `{f}`: expected <task>=<ERROR_CODE>"))?;
        handler
            .failures
            .insert(task.trim().to_string(), code.trim().to_string());
    }
    let known: Vec<&str> = wfs
        .iter()
        .flat_map(|w| w.contracts.keys())
        .map(String::as_str)
        .filter(|n| !n.starts_with(orbit_ir::WORKFLOW_CONTRACT_PREFIX))
        .collect();
    for task in handler.results.keys().chain(handler.failures.keys()) {
        if !known.contains(&task.as_str()) {
            return Err(format!(
                "`{task}` is not a task {} can call; tasks: {}",
                wf.name,
                known.join(", ")
            ));
        }
    }
    let signals = args
        .signal
        .iter()
        .map(|s| parse_signal(s))
        .collect::<Result<Vec<_>, _>>()?;
    let mut sampled: Vec<String> = vec![];
    let sim = {
        let mut tracking = Tracking {
            inner: &mut handler,
            sampled: &mut sampled,
        };
        simulate(
            &wfs,
            &wf.name,
            Some(&wf.version),
            args.input,
            &signals,
            &mut tracking,
        )?
    };
    sampled.sort();
    sampled.dedup();
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "workflow": wf.name,
                "version": wf.version,
                "status": sim.root.state.status.as_str(),
                "output": sim.root.state.output,
                "error": sim.root.state.error,
                "simulated_ms": sim.elapsed_ms,
                "sampled_tasks": sampled,
                "problems": sim.problems,
                "unsent_signals": sim.unsent_signals,
                "execution": sim.root.state,
                "history": sim.root.history,
                "children": sim.children,
            }))
            .unwrap()
        );
    } else {
        print_simulation(&wf, &sim, &sampled, tty);
    }
    Ok(
        if sim.root.state.status == orbit_runtime::model::WorkflowStatus::Completed {
            ExitCode::SUCCESS
        } else {
            ExitCode::from(1)
        },
    )
}

fn print_simulation(wf: &WorkflowIr, sim: &Simulation, sampled: &[String], tty: bool) {
    let err = crate::color_err();
    eprintln!(
        "{} {}@{} ({} simulated, no server or workers)",
        paint("simulated", view::CYAN, err),
        wf.name,
        wf.version,
        simulated(sim.elapsed_ms)
    );
    if !sampled.is_empty() {
        eprintln!(
            "{} generated sample results for: {}",
            paint("note:", view::DIM, err),
            sampled.join(", ")
        );
    }
    for p in &sim.problems {
        eprintln!("{} {p}", paint("warning:", view::YELLOW, err));
    }
    for s in &sim.unsent_signals {
        eprintln!(
            "{} signal `{s}` was not sent: the execution never waited for it",
            paint("warning:", view::YELLOW, err)
        );
    }
    println!();
    print!("{}", timeline(&sim.root.history, tty));
    for child in &sim.children {
        println!(
            "\nchild execution {} ({}):",
            child.state.id, child.state.workflow
        );
        print!("{}", indent(&timeline(&child.history, tty), "  "));
    }
    println!();
    let state = serde_json::to_value(&sim.root.state).unwrap_or_default();
    print!("{}", view::status(&state, tty));
}

/// Remembers which tasks got a generated sample result.
struct Tracking<'a> {
    inner: &'a mut SampleHandler,
    sampled: &'a mut Vec<String>,
}

impl orbit_runtime::testing::TaskHandler for Tracking<'_> {
    fn handle(
        &mut self,
        call: &orbit_runtime::testing::TaskCall,
    ) -> orbit_runtime::testing::TaskOutcome {
        let task = &call.assignment.task_name;
        if !self.inner.results.contains_key(task) && !self.inner.failures.contains_key(task) {
            self.sampled.push(task.clone());
        }
        self.inner.handle(call)
    }
}
