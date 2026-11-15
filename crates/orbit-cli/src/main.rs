//! `orbit` — compiler driver, runtime server and runtime client.

mod client;
mod compat;
mod lsp;
mod replay;
mod scaffold;
mod testing;
mod view;

use clap::{Args, Parser, Subcommand, ValueEnum};
use client::Client;
use orbit_compiler::{compile_files, Compilation};
use orbit_ir::WorkflowIr;
use serde_json::{json, Value};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(Parser)]
#[command(name = "orbit", version, about = "ORBIT — polyglot orchestration language", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Args, Clone)]
struct Remote {
    /// Runtime API address.
    #[arg(
        long,
        env = "ORBIT_SERVER",
        default_value = "http://127.0.0.1:7233",
        global = true
    )]
    server: String,
    /// Bearer token for the runtime API.
    #[arg(long, env = "ORBIT_TOKEN", global = true, hide_env_values = true)]
    token: Option<String>,
    /// Extra PEM CA certificate to trust (for a runtime with a private certificate).
    #[arg(long, env = "ORBIT_CA_CERT", global = true)]
    ca_cert: Option<PathBuf>,
    /// PEM client certificate for mutual TLS (with --client-key).
    #[arg(
        long,
        env = "ORBIT_CLIENT_CERT",
        global = true,
        requires = "client_key"
    )]
    client_cert: Option<PathBuf>,
    /// PEM private key for --client-cert.
    #[arg(
        long,
        env = "ORBIT_CLIENT_KEY",
        global = true,
        requires = "client_cert"
    )]
    client_key: Option<PathBuf>,
}

impl Remote {
    fn client(&self) -> Client {
        let ca = self.ca_cert.as_ref().map(|p| {
            std::fs::read(p).unwrap_or_else(|e| {
                eprintln!("error: cannot read CA certificate {}: {e}", p.display());
                std::process::exit(2)
            })
        });
        let read = |p: &PathBuf| {
            std::fs::read(p).unwrap_or_else(|e| {
                eprintln!("error: cannot read {}: {e}", p.display());
                std::process::exit(2)
            })
        };
        let identity = self
            .client_cert
            .as_ref()
            .zip(self.client_key.as_ref())
            .map(|(c, k)| (read(c), read(k)));
        Client::new(
            &self.server,
            self.token.clone().filter(|t| !t.is_empty()),
            ca,
            identity,
        )
    }
}

#[derive(Clone, Copy, ValueEnum, PartialEq)]
enum DiagFormat {
    Human,
    Json,
}

#[derive(Clone, Copy, ValueEnum)]
enum GraphFormat {
    Mermaid,
    Dot,
}

#[derive(Subcommand)]
enum Command {
    /// Create a new ORBIT project with an example workflow and workers.
    Init { name: PathBuf },
    /// Type-check workflow files without producing output.
    Check {
        files: Vec<PathBuf>,
        #[arg(long, value_enum, default_value = "human")]
        format: DiagFormat,
        /// Treat warnings (missing compensations, unreachable steps, unused
        /// signals, waits without a timeout, ...) as errors.
        #[arg(long)]
        strict: bool,
    },
    /// Check workflow files, and validate compiled IR (.json) files.
    Validate { files: Vec<PathBuf> },
    /// Compile workflow files to orbit-ir JSON.
    Compile {
        files: Vec<PathBuf>,
        /// Treat warnings as errors.
        #[arg(long)]
        strict: bool,
        /// Output directory.
        #[arg(short, long, default_value = "build")]
        out_dir: PathBuf,
        /// Print the IR to stdout instead of writing files.
        #[arg(long)]
        stdout: bool,
    },
    /// Format .orbit files in place (comments and line breaks are preserved).
    Fmt {
        files: Vec<PathBuf>,
        /// Do not write; exit with status 1 if any file is not formatted.
        #[arg(long)]
        check: bool,
    },
    /// Run the language server (LSP over stdio), used by editor extensions.
    Lsp,
    /// Render a workflow's step graph (Mermaid or Graphviz).
    Graph {
        file: PathBuf,
        #[arg(long)]
        workflow: Option<String>,
        #[arg(long, value_enum, default_value = "mermaid")]
        format: GraphFormat,
    },
    /// Check that new workflow and contract versions are compatible with old ones.
    Compat {
        /// The old version: a .orbit file or a compiled IR .json file.
        old: PathBuf,
        /// The new version: a .orbit file or a compiled IR .json file.
        new: PathBuf,
        /// `human` prints one line per finding; `json` prints a single report object.
        #[arg(long, value_enum, default_value = "human")]
        format: DiagFormat,
    },
    /// Run the ORBIT runtime server.
    Server(ServerArgs),
    /// Compile and register workflow definitions with the runtime.
    Deploy {
        files: Vec<PathBuf>,
        #[command(flatten)]
        remote: Remote,
    },
    /// Deploy (if given a file) and start a workflow, then follow it to completion.
    Run {
        /// A `.orbit` file, compiled IR `.json`, or the name of a registered workflow.
        target: String,
        /// Workflow to run when the file defines several.
        #[arg(long)]
        workflow: Option<String>,
        #[arg(long)]
        version: Option<String>,
        /// Workflow input as JSON.
        #[arg(short, long)]
        input: Option<String>,
        /// Read the workflow input from a JSON file.
        #[arg(long)]
        input_file: Option<PathBuf>,
        /// Client-chosen execution id; starting the same id twice is a no-op.
        #[arg(long)]
        id: Option<String>,
        /// Return immediately after starting.
        #[arg(long)]
        no_wait: bool,
        #[command(flatten)]
        remote: Remote,
    },
    /// Show an execution's status and steps.
    Status {
        execution_id: String,
        #[command(flatten)]
        remote: Remote,
    },
    /// Inspect a compiled IR file, or an execution's full state.
    Inspect {
        target: String,
        #[command(flatten)]
        remote: Remote,
    },
    /// Show an execution's event history as a timeline.
    Logs {
        execution_id: String,
        /// Print raw JSON events (with sensitive fields redacted).
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        remote: Remote,
    },
    /// Request cancellation of an execution (runs compensations).
    Cancel {
        execution_id: String,
        #[arg(long)]
        reason: Option<String>,
        #[command(flatten)]
        remote: Remote,
    },
    /// Send a typed signal to a running execution.
    Signal {
        execution_id: String,
        /// The signal name declared in the workflow (`signal <name> { ... }`).
        name: String,
        /// Payload as JSON (default `{}`); `@file.json` reads a file.
        #[arg(long, default_value = "{}")]
        data: String,
        #[command(flatten)]
        remote: Remote,
    },
    /// Evaluate a query declared in the workflow against an execution.
    Query {
        execution_id: String,
        /// The query name declared in the workflow (`query <name> = ...`).
        name: String,
        #[command(flatten)]
        remote: Remote,
    },
    /// List recent executions.
    Ps {
        #[arg(long)]
        status: Option<String>,
        #[arg(long)]
        workflow: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: u32,
        #[command(flatten)]
        remote: Remote,
    },
    /// Generate a random bearer token and the workers-file entry for it.
    Token {
        /// Worker id for the generated `[[worker]]` entry.
        #[arg(long, default_value = "my-worker")]
        worker: String,
        /// Task patterns the worker may serve.
        #[arg(long = "task", default_value = "*")]
        tasks: Vec<String>,
    },
    /// Worker commands.
    Workers {
        #[command(subcommand)]
        command: WorkersCommand,
    },
    /// Run the `test` declarations in .orbit files, without a server or workers.
    Test {
        /// Files with `test` declarations (and the workflows they import).
        files: Vec<PathBuf>,
        /// `human` prints one line per test; `json` prints a single report object.
        #[arg(long, value_enum, default_value = "human")]
        format: DiagFormat,
        /// Only run tests whose name contains this text.
        #[arg(long)]
        filter: Option<String>,
    },
    /// Run a workflow locally with mocked or generated task results.
    Simulate {
        /// A `.orbit` file or compiled IR `.json`.
        file: PathBuf,
        /// Workflow to run when the file defines several.
        #[arg(long)]
        workflow: Option<String>,
        /// Workflow input as JSON.
        #[arg(short, long)]
        input: Option<String>,
        /// Read the workflow input from a JSON file.
        #[arg(long)]
        input_file: Option<PathBuf>,
        /// JSON file mapping task names to the results they return.
        #[arg(long)]
        mocks: Option<PathBuf>,
        /// Make a task fail permanently: `<task>=<ERROR_CODE>` (repeatable).
        #[arg(long, value_name = "TASK=CODE")]
        fail: Vec<String>,
        /// Send a signal once the execution waits for it: `<name>='<json>'` (repeatable, in order).
        #[arg(long, value_name = "NAME=JSON")]
        signal: Vec<String>,
        /// Print the final state, history and child executions as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Re-run a recorded history through the current decider and report divergences.
    Replay {
        /// Execution to replay; its history and definition are fetched from the runtime.
        execution_id: Option<String>,
        /// Replay a history file instead (`GET /v1/executions/{id}/history` output).
        #[arg(long, conflicts_with = "execution_id")]
        history: Option<PathBuf>,
        /// The definition to replay against: compiled IR `.json` or `.orbit` source
        /// (required with --history; overrides the runtime's copy otherwise).
        #[arg(long)]
        definition: Option<PathBuf>,
        /// Workflow to use when --definition defines several (default: the one the history ran).
        #[arg(long)]
        workflow: Option<String>,
        /// Print the reconstructed execution state right after this event.
        #[arg(long, value_name = "SEQ")]
        at: Option<u64>,
        /// Print the report as JSON.
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        remote: Remote,
    },
}

#[derive(Subcommand)]
enum WorkersCommand {
    /// List registered workers.
    List {
        #[command(flatten)]
        remote: Remote,
    },
}

#[derive(Args)]
struct ServerArgs {
    /// Where definitions and execution history live: a SQLite file path, or a
    /// PostgreSQL URL such as `postgres://user:pass@host:5432/orbit`.
    #[arg(
        long,
        env = "ORBIT_DB",
        default_value = "orbit.db",
        hide_env_values = true
    )]
    db: String,
    #[arg(long, env = "ORBIT_ADDR", default_value = "127.0.0.1:7233")]
    addr: std::net::SocketAddr,
    /// Export OpenTelemetry traces (OTLP/HTTP, JSON) to this collector, e.g.
    /// http://localhost:4318. Honours OTEL_EXPORTER_OTLP_HEADERS and OTEL_SERVICE_NAME.
    #[arg(long, env = "OTEL_EXPORTER_OTLP_ENDPOINT")]
    otlp_endpoint: Option<String>,
    /// Also serve the gRPC binding of the worker protocol on this address
    /// (same credentials and TLS settings as the HTTP API).
    #[arg(long, env = "ORBIT_GRPC_ADDR")]
    grpc_addr: Option<std::net::SocketAddr>,
    /// Require this bearer token on every API call (except /v1/health).
    #[arg(long, env = "ORBIT_TOKEN", hide_env_values = true)]
    token: Option<String>,
    /// Allow binding to a non-loopback address without a token.
    #[arg(long)]
    insecure: bool,
    /// Second credential that may only use the worker protocol (`/v1/workers/*`, `/v1/tasks/*`).
    /// Give this to worker hosts instead of the admin token.
    #[arg(long, env = "ORBIT_WORKER_TOKEN", hide_env_values = true)]
    worker_token: Option<String>,
    /// PEM certificate chain; with --tls-key, serve HTTPS.
    #[arg(long, env = "ORBIT_TLS_CERT", requires = "tls_key")]
    tls_cert: Option<PathBuf>,
    /// PEM private key for --tls-cert.
    #[arg(long, env = "ORBIT_TLS_KEY", requires = "tls_cert")]
    tls_key: Option<PathBuf>,
    /// Enable mutual TLS: verify client certificates signed by this PEM CA.
    /// Certificates identify workers declared with `client_cert` in --workers-file.
    #[arg(long, env = "ORBIT_TLS_CLIENT_CA", requires = "tls_cert")]
    tls_client_ca: Option<PathBuf>,
    /// TOML file declaring per-worker identities: id, token hash or client
    /// certificate name, allowed task patterns. See docs/src/runtime/security.md.
    #[arg(long, env = "ORBIT_WORKERS_FILE")]
    workers_file: Option<PathBuf>,
    /// Execute `orbit.http.request` steps inside the runtime.
    #[arg(long)]
    enable_http_adapter: bool,
    /// Maximum running executions; further starts get 429 RESOURCE_EXHAUSTED.
    #[arg(long, env = "ORBIT_MAX_ACTIVE_EXECUTIONS")]
    max_active_executions: Option<usize>,
    /// Maximum elements one `for` step may fan out to (larger lists fail the step with LIMIT_EXCEEDED).
    #[arg(long, env = "ORBIT_MAX_FAN_OUT")]
    max_fan_out: Option<usize>,
    /// Maximum history events per execution (past it the execution fails with LIMIT_EXCEEDED).
    #[arg(long, env = "ORBIT_MAX_HISTORY_EVENTS")]
    max_history_events: Option<u64>,
    /// Maximum size in bytes of an execution input, task result or signal payload (413 PAYLOAD_TOO_LARGE).
    #[arg(long, env = "ORBIT_MAX_PAYLOAD_BYTES")]
    max_payload_bytes: Option<usize>,
    /// Seconds without contact after which a worker is considered lost.
    #[arg(long, default_value_t = 30)]
    session_timeout: u64,
    /// Maximum request body size in bytes.
    #[arg(long, default_value_t = 4 * 1024 * 1024)]
    max_body: usize,
    /// Log format.
    #[arg(long, default_value = "text", value_parser = ["text", "json"])]
    log_format: String,
    /// Run as one of several servers on the same PostgreSQL database. By
    /// default one leader serves and the others stand by and take over if it
    /// fails; see --cluster-mode.
    #[arg(long, env = "ORBIT_CLUSTER")]
    cluster: bool,
    /// With --cluster: `standby` (one leader serves, the others stand by) or
    /// `shared` (every server is active and they share executions and tasks).
    #[arg(long, env = "ORBIT_CLUSTER_MODE", default_value = "standby", value_parser = ["standby", "shared"])]
    cluster_mode: String,
    /// Shared mode: seconds without a heartbeat after which a server is
    /// considered dead and the others take over its executions (minimum 2).
    #[arg(long, env = "ORBIT_NODE_TIMEOUT", default_value_t = 5)]
    node_timeout: u64,
    /// This server's name in the cluster (default: the host name).
    #[arg(long, env = "ORBIT_NODE_ID", requires = "cluster")]
    node_id: Option<String>,
    /// URL other servers give out to reach this one while it leads, and that
    /// shared mode lists in /v1/health, e.g. http://orbit-0.orbit:7233
    /// (default: derived from --addr when it is not a wildcard address).
    #[arg(long, env = "ORBIT_ADVERTISE_ADDR", requires = "cluster")]
    advertise_addr: Option<String>,
}

fn main() -> ExitCode {
    // Exit quietly when stdout is closed early (`orbit logs --json | head`)
    // instead of panicking on a broken pipe, as other Unix tools do.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let cli = Cli::parse();
    match run(cli.command) {
        Ok(code) => code,
        Err(e) => {
            eprintln!(
                "{} {e}",
                view::paint("error:", view::RED, std::io::stderr().is_terminal())
            );
            ExitCode::from(1)
        }
    }
}

type Res = Result<ExitCode, String>;

fn color_err() -> bool {
    std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none()
}

/// Compile and print diagnostics; `None` when compilation failed.
fn compile(files: &[PathBuf], format: DiagFormat) -> Result<Compilation, String> {
    if files.is_empty() {
        return Err("no input files (expected one or more .orbit files)".into());
    }
    let c = compile_files(files);
    match format {
        DiagFormat::Json => {
            let diags: Vec<_> = c
                .diagnostics
                .iter()
                .map(|d| d.to_json(&c.sources))
                .collect();
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({ "diagnostics": diags })).unwrap()
            );
        }
        DiagFormat::Human => {
            if !c.diagnostics.is_empty() {
                eprintln!("{}", c.render_diagnostics(color_err()));
            }
        }
    }
    Ok(c)
}

fn summarize_errors(c: &Compilation) -> String {
    let errors = c.diagnostics.iter().filter(|d| d.is_error()).count();
    let warnings = c.diagnostics.len() - errors;
    format!("compilation failed: {errors} error(s), {warnings} warning(s)")
}

fn load_ir(path: &Path) -> Result<WorkflowIr, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    WorkflowIr::from_json(&text).map_err(|e| format!("{}: {e}", path.display()))
}

fn workflows_from(target: &Path) -> Result<Vec<WorkflowIr>, String> {
    if target.extension().is_some_and(|e| e == "json") {
        return Ok(vec![load_ir(target)?]);
    }
    let c = compile(&[target.to_path_buf()], DiagFormat::Human)?;
    if c.has_errors() {
        return Err(summarize_errors(&c));
    }
    Ok(c.workflows)
}

fn run(cmd: Command) -> Res {
    let tty = std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
    match cmd {
        Command::Init { name } => scaffold::init(&name).map(|_| ExitCode::SUCCESS),
        Command::Check {
            files,
            format,
            strict,
        } => {
            let c = compile(&files, format)?;
            if strict && !c.diagnostics.is_empty() && !c.has_errors() {
                if format == DiagFormat::Human {
                    return Err(format!(
                        "{} warning(s) treated as errors (--strict)",
                        c.diagnostics.len()
                    ));
                }
                return Ok(ExitCode::from(1));
            }
            if c.has_errors() {
                if format == DiagFormat::Human {
                    return Err(summarize_errors(&c));
                }
                return Ok(ExitCode::from(1));
            }
            if format == DiagFormat::Human {
                for wf in &c.workflows {
                    println!(
                        "{} {}@{} ({} steps)",
                        view::paint("ok", view::GREEN, tty),
                        wf.name,
                        wf.version,
                        wf.steps.len()
                    );
                }
                if c.workflows.is_empty() {
                    println!(
                        "{} no workflows (contracts only)",
                        view::paint("ok", view::GREEN, tty)
                    );
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Validate { files } => {
            let (ir_files, sources): (Vec<_>, Vec<_>) = files
                .into_iter()
                .partition(|f| f.extension().is_some_and(|e| e == "json"));
            for f in &ir_files {
                let ir = load_ir(f)?;
                println!(
                    "{} {} is valid {} ({}@{}, {})",
                    view::paint("ok", view::GREEN, tty),
                    f.display(),
                    ir.ir_version,
                    ir.name,
                    ir.version,
                    ir.definition_hash
                );
            }
            if !sources.is_empty() {
                return run(Command::Check {
                    files: sources,
                    format: DiagFormat::Human,
                    strict: false,
                });
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Compile {
            files,
            strict,
            out_dir,
            stdout,
        } => {
            let c = compile(&files, DiagFormat::Human)?;
            if c.has_errors() {
                return Err(summarize_errors(&c));
            }
            if strict && !c.diagnostics.is_empty() {
                return Err(format!(
                    "{} warning(s) treated as errors (--strict)",
                    c.diagnostics.len()
                ));
            }
            if c.workflows.is_empty() {
                return Err("nothing to compile: no workflow declarations found".into());
            }
            for (wf, file) in c.workflows.iter().zip(&c.workflow_files) {
                let text = serde_json::to_string_pretty(wf).unwrap() + "\n";
                if stdout {
                    print!("{text}");
                    continue;
                }
                let stem = Path::new(&c.sources.file(*file).name)
                    .file_stem()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default();
                let count = c.workflow_files.iter().filter(|f| *f == file).count();
                let name = if count > 1 {
                    format!("{stem}.{}.orbit.json", wf.name)
                } else {
                    format!("{stem}.orbit.json")
                };
                std::fs::create_dir_all(&out_dir).map_err(|e| e.to_string())?;
                let path = out_dir.join(name);
                std::fs::write(&path, text)
                    .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
                eprintln!(
                    "{} {}@{} -> {}",
                    view::paint("compiled", view::GREEN, color_err()),
                    wf.name,
                    wf.version,
                    path.display()
                );
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Graph {
            file,
            workflow,
            format,
        } => {
            let wfs = workflows_from(&file)?;
            let wf = pick(&wfs, workflow.as_deref())?;
            print!(
                "{}",
                match format {
                    GraphFormat::Mermaid => view::mermaid(wf),
                    GraphFormat::Dot => view::dot(wf),
                }
            );
            Ok(ExitCode::SUCCESS)
        }
        Command::Compat { old, new, format } => Ok(compat::run(&old, &new, format)),
        Command::Server(args) => server(args),
        Command::Deploy { files, remote } => {
            let client = remote.client();
            for f in files {
                for wf in workflows_from(&f)? {
                    deploy(&client, &wf, tty)?;
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Run {
            target,
            workflow,
            version,
            input,
            input_file,
            id,
            no_wait,
            remote,
        } => {
            let client = remote.client();
            let (name, version) = if target.ends_with(".orbit") || target.ends_with(".json") {
                let wfs = workflows_from(Path::new(&target))?;
                let wf = pick(&wfs, workflow.as_deref())?;
                // Register every workflow in the file (children come first) so
                // that `workflow "Name"` steps can start their children.
                for w in &wfs {
                    deploy(&client, w, tty)?;
                }
                (wf.name.clone(), Some(wf.version.clone()))
            } else {
                (target, version)
            };
            let input: Value = match (input, input_file) {
                (Some(s), _) => serde_json::from_str(&s)
                    .map_err(|e| format!("--input is not valid JSON: {e}"))?,
                (None, Some(p)) => {
                    let text = std::fs::read_to_string(&p)
                        .map_err(|e| format!("cannot read {}: {e}", p.display()))?;
                    serde_json::from_str(&text)
                        .map_err(|e| format!("{} is not valid JSON: {e}", p.display()))?
                }
                (None, None) => json!({}),
            };
            let (_, started) = client
                .post("/v1/executions", &json!({ "workflow": name, "version": version, "input": input, "execution_id": id }))
                .map_err(|e| e.to_string())?;
            let exec_id = started["execution_id"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let verb = if started["created"].as_bool() == Some(false) {
                "attached to existing"
            } else {
                "started"
            };
            eprintln!(
                "{} execution {}",
                view::paint(verb, view::CYAN, color_err()),
                exec_id
            );
            if no_wait {
                println!("{exec_id}");
                return Ok(ExitCode::SUCCESS);
            }
            view::follow(&client, &exec_id, tty)
        }
        Command::Status {
            execution_id,
            remote,
        } => {
            let s = remote
                .client()
                .get(&format!("/v1/executions/{execution_id}"))
                .map_err(|e| e.to_string())?;
            print!("{}", view::status(&s, tty));
            Ok(ExitCode::SUCCESS)
        }
        Command::Inspect { target, remote } => {
            let p = Path::new(&target);
            if p.exists() {
                let ir = if target.ends_with(".json") {
                    load_ir(p)?
                } else {
                    pick(&workflows_from(p)?, None)?.clone()
                };
                print!("{}", view::ir_summary(&ir, tty));
            } else {
                let s = remote
                    .client()
                    .get(&format!("/v1/executions/{target}"))
                    .map_err(|e| e.to_string())?;
                println!(
                    "{}",
                    serde_json::to_string_pretty(&view::redact(s)).unwrap()
                );
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Logs {
            execution_id,
            json,
            remote,
        } => {
            let h = remote
                .client()
                .get(&format!("/v1/executions/{execution_id}/history"))
                .map_err(|e| e.to_string())?;
            if json {
                for e in h["events"].as_array().cloned().unwrap_or_default() {
                    println!("{}", serde_json::to_string(&view::redact(e)).unwrap());
                }
            } else {
                print!("{}", view::timeline(&h, tty));
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Signal {
            execution_id,
            name,
            data,
            remote,
        } => {
            let text = match data.strip_prefix('@') {
                Some(path) => {
                    std::fs::read_to_string(path).map_err(|e| format!("cannot read {path}: {e}"))?
                }
                None => data,
            };
            let payload: Value = serde_json::from_str(&text)
                .map_err(|e| format!("--data is not valid JSON: {e}"))?;
            let (_, r) = remote
                .client()
                .post(
                    &format!("/v1/executions/{execution_id}/signals/{name}"),
                    &payload,
                )
                .map_err(|e| e.to_string())?;
            println!(
                "signal `{name}` recorded for {execution_id} (history #{}); the next `await {name}` step takes it",
                r["seq"]
            );
            Ok(ExitCode::SUCCESS)
        }
        Command::Query {
            execution_id,
            name,
            remote,
        } => {
            let r = remote
                .client()
                .get(&format!("/v1/executions/{execution_id}/queries/{name}"))
                .map_err(|e| e.to_string())?;
            println!("{}", serde_json::to_string_pretty(&r["value"]).unwrap());
            Ok(ExitCode::SUCCESS)
        }
        Command::Cancel {
            execution_id,
            reason,
            remote,
        } => {
            let (_, s) = remote
                .client()
                .post(
                    &format!("/v1/executions/{execution_id}/cancel"),
                    &json!({ "reason": reason }),
                )
                .map_err(|e| e.to_string())?;
            println!(
                "cancellation requested; execution is {}",
                s["status"].as_str().unwrap_or("?")
            );
            Ok(ExitCode::SUCCESS)
        }
        Command::Ps {
            status,
            workflow,
            limit,
            remote,
        } => {
            let mut q = format!("/v1/executions?limit={limit}");
            if let Some(s) = status {
                q.push_str(&format!("&status={s}"));
            }
            if let Some(w) = workflow {
                q.push_str(&format!("&workflow={w}"));
            }
            let r = remote.client().get(&q).map_err(|e| e.to_string())?;
            print!("{}", view::executions(&r, tty));
            Ok(ExitCode::SUCCESS)
        }
        Command::Fmt { files, check } => {
            if files.is_empty() {
                return Err("no input files".into());
            }
            let mut unformatted = 0;
            for f in &files {
                let src = std::fs::read_to_string(f)
                    .map_err(|e| format!("cannot read {}: {e}", f.display()))?;
                let out = orbit_syntax::format::format_source(&src)
                    .map_err(|e| format!("{}: {e}", f.display()))?;
                if out != src {
                    unformatted += 1;
                    if check {
                        eprintln!(
                            "{} {}",
                            view::paint("would reformat", view::YELLOW, color_err()),
                            f.display()
                        );
                    } else {
                        std::fs::write(f, out)
                            .map_err(|e| format!("cannot write {}: {e}", f.display()))?;
                        eprintln!(
                            "{} {}",
                            view::paint("formatted", view::GREEN, color_err()),
                            f.display()
                        );
                    }
                }
            }
            if check && unformatted > 0 {
                return Ok(ExitCode::from(1));
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Lsp => lsp::run().map(|_| ExitCode::SUCCESS),
        Command::Token { worker, tasks } => {
            // Two v4 UUIDs from the OS CSPRNG: 244 random bits.
            let token = format!(
                "{}{}",
                uuid::Uuid::new_v4().simple(),
                uuid::Uuid::new_v4().simple()
            );
            let tasks = tasks
                .iter()
                .map(|t| format!("{t:?}"))
                .collect::<Vec<_>>()
                .join(", ");
            println!("{token}");
            eprintln!(
                "\nGive the token above to the worker (ORBIT_WORKER_TOKEN). Add this to the workers file:\n\n[[worker]]\nid = {worker:?}\ntoken_sha256 = {:?}\ntasks = [{tasks}]\n",
                orbit_runtime::identity::sha256_hex(&token)
            );
            Ok(ExitCode::SUCCESS)
        }
        Command::Test {
            files,
            format,
            filter,
        } => testing::test(&files, format, filter.as_deref(), tty),
        Command::Simulate {
            file,
            workflow,
            input,
            input_file,
            mocks,
            fail,
            signal,
            json,
        } => testing::simulate_cmd(
            testing::SimulateArgs {
                file: &file,
                workflow: workflow.as_deref(),
                input: parse_input(input, input_file)?,
                mocks: mocks.as_deref(),
                fail: &fail,
                signal: &signal,
                json,
            },
            tty,
        ),
        Command::Replay {
            execution_id,
            history,
            definition,
            workflow,
            at,
            json,
            remote,
        } => replay::run(
            replay::ReplayArgs {
                execution_id,
                history,
                definition,
                workflow,
                at,
                json,
                remote,
            },
            tty,
        ),
        Command::Workers {
            command: WorkersCommand::List { remote },
        } => {
            let r = remote
                .client()
                .get("/v1/workers")
                .map_err(|e| e.to_string())?;
            print!("{}", view::workers(&r, tty));
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// `--input '<json>'` or `--input-file <path>`; `{}` when neither is given.
fn parse_input(input: Option<String>, input_file: Option<PathBuf>) -> Result<Value, String> {
    match (input, input_file) {
        (Some(s), _) => {
            serde_json::from_str(&s).map_err(|e| format!("--input is not valid JSON: {e}"))
        }
        (None, Some(p)) => {
            let text = std::fs::read_to_string(&p)
                .map_err(|e| format!("cannot read {}: {e}", p.display()))?;
            serde_json::from_str(&text)
                .map_err(|e| format!("{} is not valid JSON: {e}", p.display()))
        }
        (None, None) => Ok(json!({})),
    }
}

/// Workflows that no other workflow in the set starts as a child.
fn roots(wfs: &[WorkflowIr]) -> Vec<&WorkflowIr> {
    wfs.iter()
        .filter(|w| {
            let key = format!("{}{}", orbit_ir::WORKFLOW_CONTRACT_PREFIX, w.name);
            !wfs.iter().any(|o| o.contracts.contains_key(&key))
        })
        .collect()
}

fn pick<'a>(wfs: &'a [WorkflowIr], name: Option<&str>) -> Result<&'a WorkflowIr, String> {
    match name {
        Some(n) => wfs.iter().find(|w| w.name == n).ok_or_else(|| {
            format!(
                "no workflow `{n}`; available: {}",
                wfs.iter()
                    .map(|w| w.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }),
        None => match wfs {
            [one] => Ok(one),
            // A parent and its children: pick the one workflow nobody calls.
            many if roots(many).len() == 1 => Ok(roots(many)[0]),
            [] => Err("the file defines no workflows".into()),
            many => Err(format!(
                "the file defines several workflows; choose one with --workflow ({})",
                many.iter()
                    .map(|w| w.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        },
    }
}

fn deploy(client: &Client, wf: &WorkflowIr, tty: bool) -> Result<(), String> {
    let (status, _) = client
        .post_raw("/v1/workflows", serde_json::to_string(wf).unwrap())
        .map_err(|e| e.to_string())?;
    let verb = if status == 201 {
        "deployed"
    } else {
        "up to date"
    };
    eprintln!(
        "{} {}@{} ({})",
        view::paint(verb, view::GREEN, color_err() && tty),
        wf.name,
        wf.version,
        &wf.definition_hash[..19.min(wf.definition_hash.len())]
    );
    Ok(())
}

/// `postgres://user:secret@host/db` -> `postgres://user:***@host/db` for logs.
fn redact_url(s: &str) -> String {
    match (s.find("://"), s.rfind('@')) {
        (Some(scheme), Some(at)) if at > scheme => {
            let creds = &s[scheme + 3..at];
            match creds.find(':') {
                Some(colon) => format!("{}{}:***{}", &s[..scheme + 3], &creds[..colon], &s[at..]),
                None => s.to_string(),
            }
        }
        _ => s.to_string(),
    }
}

fn server(mut args: ServerArgs) -> Res {
    // An empty token (e.g. `ORBIT_TOKEN=`) must not count as authentication.
    args.token = args.token.filter(|t| !t.trim().is_empty());
    args.worker_token = args.worker_token.filter(|t| !t.trim().is_empty());
    if args.worker_token.is_some() && args.token.is_none() {
        return Err(
            "--worker-token requires --token: without an admin token every endpoint would be open"
                .into(),
        );
    }
    if args.worker_token.is_some() && args.worker_token == args.token {
        return Err("--worker-token must differ from --token".into());
    }
    let identities = match &args.workers_file {
        Some(path) => orbit_runtime::identity::Identities::load(path)?,
        None => Default::default(),
    };
    if !identities.is_empty() && args.token.is_none() {
        return Err("--workers-file requires --token: without an admin token administrative endpoints would be open".into());
    }
    if args.tls_client_ca.is_some() && identities.is_empty() {
        return Err("--tls-client-ca needs --workers-file entries with `client_cert` to map certificates to workers".into());
    }
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_env("ORBIT_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    if args.log_format == "json" {
        tracing_subscriber::fmt()
            .json()
            .with_env_filter(filter)
            .with_writer(std::io::stderr)
            .init();
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(std::io::stderr)
            .init();
    }
    if !args.addr.ip().is_loopback() && args.token.is_none() && !args.insecure {
        return Err(format!(
            "refusing to listen on {} without authentication; pass --token (or ORBIT_TOKEN), or --insecure for trusted networks",
            args.addr
        ));
    }
    let db_label = redact_url(&args.db);
    let is_postgres = args.db.starts_with("postgres://") || args.db.starts_with("postgresql://");
    if args.cluster && !is_postgres {
        return Err("--cluster requires a PostgreSQL database (--db postgres://...): the servers share it for leader election and history".into());
    }
    let shared_mode = args.cluster_mode == "shared";
    if shared_mode && !args.cluster {
        return Err("--cluster-mode shared requires --cluster".into());
    }
    // In cluster mode the node starts as a standby with a read-only store; in
    // shared mode every node writes the executions it owns.
    let store = if args.cluster && shared_mode {
        orbit_runtime::store::PgStore::connect_with(&args.db, orbit_runtime::store::PgMode::Shared)
            .map(orbit_runtime::Store::Postgres)
    } else if args.cluster {
        orbit_runtime::store::PgStore::connect_with(
            &args.db,
            orbit_runtime::store::PgMode::ReadOnly,
        )
        .map(orbit_runtime::Store::Postgres)
    } else {
        orbit_runtime::Store::connect(&args.db)
    }
    .map_err(|e| format!("cannot open database {db_label}: {e}"))?;
    let config = orbit_runtime::EngineConfig {
        session_timeout_ms: args.session_timeout.max(1) * 1000,
        heartbeat_interval_ms: (args.session_timeout.max(1) * 1000 / 6).max(500),
        http_adapter: args.enable_http_adapter,
        limits: orbit_runtime::engine::Limits {
            max_active_executions: args.max_active_executions,
            max_fan_out: args.max_fan_out,
            max_history_events: args.max_history_events,
            max_payload_bytes: args.max_payload_bytes,
        },
    };
    let store_backend = store.backend();
    let engine = orbit_runtime::Engine::open(store, config, orbit_runtime::engine::system_clock())
        .map_err(|e| e.to_string())?;
    let mut engine = engine;
    if let Some(endpoint) = args.otlp_endpoint.as_deref().filter(|e| !e.is_empty()) {
        engine.enable_telemetry(orbit_runtime::telemetry::TelemetryConfig::new(endpoint));
    }
    let node_id = args
        .node_id
        .clone()
        .unwrap_or_else(orbit_runtime::cluster::default_node_id);
    let advertise_addr = args.advertise_addr.clone().or_else(|| {
        let scheme = if args.tls_cert.is_some() {
            "https"
        } else {
            "http"
        };
        (!args.addr.ip().is_unspecified()).then(|| format!("{scheme}://{}", args.addr))
    });
    if shared_mode {
        let mut c = orbit_runtime::shared::SharedConfig::new(&args.db, &node_id);
        c.advertise_addr = advertise_addr.clone();
        c.node_timeout = std::time::Duration::from_secs(args.node_timeout.max(2));
        engine = orbit_runtime::shared::attach(engine, c)
            .map_err(|e| format!("cannot join the shared cluster: {e}"))?;
    }
    let cluster = (args.cluster && !shared_mode).then(|| {
        let scheme = if args.tls_cert.is_some() {
            "https"
        } else {
            "http"
        };
        let mut c = orbit_runtime::cluster::ClusterConfig::new(
            &args.db,
            &args
                .node_id
                .clone()
                .unwrap_or_else(orbit_runtime::cluster::default_node_id),
        );
        c.advertise_addr = args.advertise_addr.clone().or_else(|| {
            (!args.addr.ip().is_unspecified()).then(|| format!("{scheme}://{}", args.addr))
        });
        let engine_config = engine.config.clone();
        let otlp = args.otlp_endpoint.clone().filter(|e| !e.is_empty());
        orbit_runtime::cluster::Node::new(
            c,
            Box::new(move |store| {
                let mut e = orbit_runtime::Engine::open(
                    store,
                    engine_config.clone(),
                    orbit_runtime::engine::system_clock(),
                )?;
                if let Some(endpoint) = otlp.as_deref() {
                    e.enable_telemetry(orbit_runtime::telemetry::TelemetryConfig::new(endpoint));
                }
                Ok(e)
            }),
        )
    });
    tracing::info!(db = %db_label, backend = store_backend, http_adapter = args.enable_http_adapter, auth = args.token.is_some(), "starting ORBIT runtime");
    let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    rt.block_on(async move {
        let shutdown = async {
            shutdown_signal().await;
            tracing::info!("shutting down");
        };
        orbit_runtime::server::serve_node(
            engine,
            orbit_runtime::server::ServerConfig {
                addr: args.addr,
                auth_token: args.token,
                worker_token: args.worker_token,
                identities,
                grpc_addr: args.grpc_addr,
                listener: None,
                grpc_listener: None,
                tls: args.tls_cert.zip(args.tls_key).map(|(cert, key)| {
                    orbit_runtime::server::TlsConfig {
                        cert,
                        key,
                        client_ca: args.tls_client_ca,
                    }
                }),
                body_limit: args.max_body,
                tick_interval: std::time::Duration::from_millis(100),
            },
            cluster,
            shutdown,
        )
        .await
    })
    .map_err(|e| e.to_string())?;
    Ok(ExitCode::SUCCESS)
}

/// Resolves on Ctrl+C (SIGINT) or, on Unix, SIGTERM, which `docker stop`,
/// Kubernetes and systemd send.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
