//! `orbit compat <old> <new>`: compatibility report between two versions.

use crate::{view, DiagFormat};
use orbit_compiler::compat::{compare, Snapshot};
use orbit_compiler::compile_files;
use orbit_ir::WorkflowIr;
use std::io::IsTerminal;
use std::path::Path;
use std::process::ExitCode;

/// Exit status for unreadable or uncompilable inputs.
const USAGE: u8 = 2;

fn color(stream_is_tty: bool) -> bool {
    stream_is_tty && std::env::var_os("NO_COLOR").is_none()
}

fn load(path: &Path) -> Result<Snapshot, String> {
    if path.extension().is_some_and(|e| e == "json") {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let ir = WorkflowIr::from_json(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        return Ok(Snapshot::from_workflows(vec![ir]));
    }
    if !path.is_file() {
        return Err(format!("cannot read {}: no such file", path.display()));
    }
    let c = compile_files(&[path.to_path_buf()]);
    if c.has_errors() {
        eprintln!(
            "{}",
            c.render_diagnostics(color(std::io::stderr().is_terminal()))
        );
        let errors = c.diagnostics.iter().filter(|d| d.is_error()).count();
        return Err(format!(
            "{}: compilation failed: {errors} error(s)",
            path.display()
        ));
    }
    Ok(Snapshot::from_compilation(&c))
}

pub fn run(old: &Path, new: &Path, format: DiagFormat) -> ExitCode {
    let err_color = color(std::io::stderr().is_terminal());
    let snapshots = load(old).and_then(|o| Ok((o, load(new)?)));
    let (o, n) = match snapshots {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{} {e}", view::paint("error:", view::RED, err_color));
            return ExitCode::from(USAGE);
        }
    };
    let report = compare(&o, &n);
    match format {
        DiagFormat::Json => println!(
            "{}",
            serde_json::to_string_pretty(&report.to_json()).unwrap()
        ),
        DiagFormat::Human => {
            print!("{}", report.render(color(std::io::stdout().is_terminal())));
            if report.errors() > 0 {
                eprintln!(
                    "{} compatibility check failed: {} error(s), {} warning(s)",
                    view::paint("error:", view::RED, err_color),
                    report.errors(),
                    report.warnings()
                );
            }
        }
    }
    if report.errors() > 0 {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}
