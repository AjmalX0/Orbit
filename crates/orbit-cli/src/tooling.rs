//! `orbit graph`, `orbit docs`, `orbit schema` and `orbit diff`.

use crate::{color_err, compile, pick, summarize_errors, view, workflows_from, DiagFormat};
use clap::ValueEnum;
use orbit_compiler::tooling::{diff, docs, graph, schema, svg};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(Clone, Copy, ValueEnum, PartialEq)]
pub enum GraphFormat {
    Mermaid,
    Dot,
    Svg,
}

#[derive(Clone, Copy, ValueEnum, PartialEq)]
pub enum DocsFormat {
    Markdown,
    Html,
}

#[derive(Clone, Copy, ValueEnum, PartialEq)]
pub enum DiffFormat {
    Text,
    Markdown,
    Json,
}

fn write(path: &Path, text: &str) -> Result<(), String> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    std::fs::write(path, text).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

fn wrote(path: &Path) {
    eprintln!(
        "{} {}",
        view::paint("wrote", view::GREEN, color_err()),
        path.display()
    );
}

pub fn graph(
    file: &Path,
    workflow: Option<&str>,
    format: GraphFormat,
    sequence: bool,
    output: Option<&Path>,
) -> Result<ExitCode, String> {
    if sequence && format != GraphFormat::Mermaid {
        return Err("--sequence produces a Mermaid sequence diagram; it cannot be combined with --format dot or svg".into());
    }
    let wfs = workflows_from(file)?;
    let wf = pick(&wfs, workflow)?;
    let text = if sequence {
        graph::sequence(wf)
    } else {
        match format {
            GraphFormat::Mermaid => graph::mermaid(wf),
            GraphFormat::Dot => graph::dot(wf),
            GraphFormat::Svg => svg::svg(wf),
        }
    };
    match output {
        Some(p) => {
            write(p, &text)?;
            wrote(p);
        }
        None => print!("{text}"),
    }
    Ok(ExitCode::SUCCESS)
}

fn compiled(files: &[PathBuf]) -> Result<orbit_compiler::Compilation, String> {
    let c = compile(files, DiagFormat::Human, &[])?;
    if c.has_errors() {
        return Err(summarize_errors(&c));
    }
    Ok(c)
}

pub fn docs(
    files: &[PathBuf],
    output: &Path,
    format: DocsFormat,
    json_schema: bool,
) -> Result<ExitCode, String> {
    let c = compiled(files)?;
    let fmt = match format {
        DocsFormat::Markdown => docs::Format::Markdown,
        DocsFormat::Html => docs::Format::Html,
    };
    let pages = docs::generate(
        &c,
        &docs::Options {
            schema_links: json_schema,
        },
    );
    for page in &pages {
        let path = output.join(format!("{}.{}", page.file, fmt.extension()));
        write(&path, &docs::render(page, fmt))?;
        wrote(&path);
    }
    if json_schema {
        let n = write_schemas(&c, &output.join("schemas"), None)?;
        eprintln!(
            "{} {n} JSON Schema file(s) in {}",
            view::paint("wrote", view::GREEN, color_err()),
            output.join("schemas").display()
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn schemas(
    c: &orbit_compiler::Compilation,
    workflow: Option<&str>,
) -> Result<std::collections::BTreeMap<String, serde_json::Value>, String> {
    match workflow {
        Some(name) => {
            let wf = pick(&c.workflows, Some(name))?;
            let mut out = schema::workflow_schemas(wf);
            out.extend(schema::all_schemas(&[], wf.contracts.values()));
            Ok(out)
        }
        None => Ok(schema::all_schemas(
            &c.workflows,
            docs::documented_contracts(c).values(),
        )),
    }
}

fn write_schemas(
    c: &orbit_compiler::Compilation,
    dir: &Path,
    workflow: Option<&str>,
) -> Result<usize, String> {
    let all = schemas(c, workflow)?;
    for (key, s) in &all {
        let path = dir.join(format!("{key}.schema.json"));
        write(&path, &(serde_json::to_string_pretty(s).unwrap() + "\n"))?;
    }
    Ok(all.len())
}

pub fn schema_cmd(
    files: &[PathBuf],
    workflow: Option<&str>,
    output: Option<&Path>,
) -> Result<ExitCode, String> {
    let c = compiled(files)?;
    match output {
        Some(dir) => {
            let n = write_schemas(&c, dir, workflow)?;
            eprintln!(
                "{} {n} JSON Schema file(s) in {}",
                view::paint("wrote", view::GREEN, color_err()),
                dir.display()
            );
        }
        None => {
            let all = schemas(&c, workflow)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::to_value(all).unwrap()).unwrap()
            );
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Exit status for unreadable or uncompilable inputs (as `orbit compat`).
const USAGE: u8 = 2;

pub fn diff_cmd(
    old: &Path,
    new: &Path,
    workflow: Option<&str>,
    format: DiffFormat,
    exit_code: bool,
) -> ExitCode {
    let snapshots = crate::compat::load(old).and_then(|o| Ok((o, crate::compat::load(new)?)));
    let (o, n) = match snapshots {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{} {e}", view::paint("error:", view::RED, color_err()));
            return ExitCode::from(USAGE);
        }
    };
    if let Some(w) = workflow {
        if !o.workflows.iter().chain(&n.workflows).any(|x| x.name == w) {
            let names: std::collections::BTreeSet<&str> = o
                .workflows
                .iter()
                .chain(&n.workflows)
                .map(|x| x.name.as_str())
                .collect();
            eprintln!(
                "{} no workflow `{w}` in either version; available: {}",
                view::paint("error:", view::RED, color_err()),
                names.into_iter().collect::<Vec<_>>().join(", ")
            );
            return ExitCode::from(USAGE);
        }
    }
    let d = diff::diff(&o, &n, workflow);
    match format {
        DiffFormat::Text => print!(
            "{}",
            d.text(std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none())
        ),
        DiffFormat::Markdown => print!("{}", d.markdown()),
        DiffFormat::Json => println!("{}", serde_json::to_string_pretty(&d.to_json()).unwrap()),
    }
    if exit_code && d.has_changes() {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}
