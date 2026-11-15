//! The gRPC binding against a real server: the full worker lifecycle,
//! value conversion, error mapping and credential enforcement.

use orbit_runtime::engine::system_clock;
use orbit_runtime::grpc::pb::worker_service_client::WorkerServiceClient;
use orbit_runtime::grpc::pb::{self, TaskContract};
use orbit_runtime::grpc::{from_json, to_json};
use orbit_runtime::identity::{sha256_hex, Identities};
use orbit_runtime::server::{serve, ServerConfig};
use orbit_runtime::{Engine, EngineConfig, Store};
use serde_json::json;
use std::time::Duration;
use tonic::metadata::MetadataValue;
use tonic::{Code, Request};

const SRC: &str = r#"
task "calc.square" { version "1.0.0" input { n: Int } output { n: Int, label: String } }
task "calc.check"  { version "1.0.0" input { n: Int } output { ok: Bool } }
workflow Calc {
    version "1.0.0"
    input { n: Int }
    step sq { worker "calc.square" with { n: input.n } }
    step check { worker "calc.check" with { n: sq.n } }
    return { squared: sq.n, label: sq.label }
}
"#;

/// Bind an ephemeral port and keep the listener, so no other test can take
/// the port between choosing it and the server starting.
fn listen() -> (std::net::TcpListener, u16) {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    (l, port)
}

/// Start a server with HTTP + gRPC; returns (http url, grpc url, execution ids).
fn start() -> (String, String, Vec<String>) {
    let ((http_l, http), (grpc_l, grpc)) = (listen(), listen());
    let c = orbit_compiler::compile_str("calc.orbit", SRC);
    assert!(!c.has_errors(), "{}", c.render_diagnostics(false));
    let mut engine = Engine::open(
        Store::in_memory().unwrap(),
        EngineConfig::default(),
        system_clock(),
    )
    .unwrap();
    engine.register_definition(c.workflows[0].clone()).unwrap();
    let ids: Vec<String> = [3, 4]
        .iter()
        .map(|n| {
            engine
                .start("Calc", None, json!({ "n": n }), None)
                .unwrap()
                .execution_id
        })
        .collect();
    let identities = Identities::parse(&format!(
        "[[worker]]\nid = \"calc-1\"\ntoken_sha256 = \"{}\"\ntasks = [\"calc.*\"]\n",
        sha256_hex("calc-token")
    ))
    .unwrap();
    std::thread::spawn(move || {
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(async move {
                let cfg = ServerConfig {
                    addr: format!("127.0.0.1:{http}").parse().unwrap(),
                    auth_token: Some("admin".into()),
                    worker_token: Some("shared-worker".into()),
                    identities,
                    tls: None,
                    grpc_addr: None,
                    listener: Some(http_l),
                    grpc_listener: Some(grpc_l),
                    body_limit: 1 << 20,
                    tick_interval: Duration::from_millis(20),
                };
                serve(engine, cfg, std::future::pending()).await.unwrap();
            });
    });
    let http_url = format!("http://127.0.0.1:{http}");
    for _ in 0..200 {
        if ureq::get(format!("{http_url}/v1/health")).call().is_ok()
            && std::net::TcpStream::connect(("127.0.0.1", grpc)).is_ok()
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    (http_url, format!("http://127.0.0.1:{grpc}"), ids)
}

fn authed<T>(msg: T, token: &str) -> Request<T> {
    let mut r = Request::new(msg);
    r.metadata_mut().insert(
        "authorization",
        MetadataValue::try_from(format!("Bearer {token}")).unwrap(),
    );
    r
}

fn contracts(names: &[&str]) -> Vec<TaskContract> {
    names
        .iter()
        .map(|n| TaskContract {
            task_name: n.to_string(),
            contract_version: "1.0.0".into(),
        })
        .collect()
}

fn execution(http: &str, id: &str) -> serde_json::Value {
    ureq::get(format!("{http}/v1/executions/{id}/wait?timeout_ms=5000"))
        .header("authorization", "Bearer admin")
        .call()
        .unwrap()
        .body_mut()
        .read_json()
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn grpc_worker_lifecycle() {
    let (http, grpc, ids) = tokio::task::spawn_blocking(start).await.unwrap();
    let mut client = WorkerServiceClient::connect(grpc).await.unwrap();

    let reg = client
        .register_worker(authed(
            pb::RegisterWorkerRequest {
                worker_id: "calc-1".into(),
                runtime: "rust".into(),
                contracts: contracts(&["calc.square", "calc.check"]),
                sdk: "grpc-test".into(),
            },
            "calc-token",
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(reg.accepted && reg.heartbeat_interval_ms > 0);
    let session = reg.worker_session_id;
    let poll = |s: String| {
        authed(
            pb::PollTaskRequest {
                worker_session_id: s,
                wait_ms: 2_000,
            },
            "calc-token",
        )
    };

    let mut done = 0;
    let mut failed_one = false;
    while done < 4 {
        let task = client
            .poll_task(poll(session.clone()))
            .await
            .unwrap()
            .into_inner();
        assert!(!task.token.is_empty(), "a task is available");
        let input = to_json(task.input.unwrap());
        let n = input["n"]
            .as_i64()
            .expect("integers survive google.protobuf.Value");
        match task.task_name.as_str() {
            "calc.square" => {
                let out = from_json(json!({ "n": n * n, "label": format!("{n} squared") }));
                let ack = client
                    .complete_task(authed(
                        pb::CompleteTaskRequest {
                            token: task.token,
                            output: Some(out),
                        },
                        "calc-token",
                    ))
                    .await
                    .unwrap();
                assert!(ack.into_inner().accepted);
            }
            _ if !failed_one => {
                failed_one = true;
                let ack = client
                    .fail_task(authed(
                        pb::FailTaskRequest {
                            token: task.token,
                            error_code: "TOO_BIG".into(),
                            error_message: format!("{n} is too big"),
                            retryable: false,
                            details: Some(from_json(json!({ "limit": 10 }))),
                        },
                        "calc-token",
                    ))
                    .await
                    .unwrap();
                assert!(ack.into_inner().accepted);
            }
            _ => {
                let out = from_json(json!({ "ok": true }));
                client
                    .complete_task(authed(
                        pb::CompleteTaskRequest {
                            token: task.token,
                            output: Some(out),
                        },
                        "calc-token",
                    ))
                    .await
                    .unwrap();
            }
        }
        done += 1;
    }
    // No more work: an empty token after wait_ms.
    let empty = client
        .poll_task(authed(
            pb::PollTaskRequest {
                worker_session_id: session.clone(),
                wait_ms: 200,
            },
            "calc-token",
        ))
        .await
        .unwrap();
    assert!(empty.into_inner().token.is_empty());

    let hb = client
        .heartbeat(authed(
            pb::HeartbeatRequest {
                worker_session_id: session.clone(),
            },
            "calc-token",
        ))
        .await
        .unwrap();
    assert!(hb.into_inner().accepted);
    client
        .unregister_worker(authed(
            pb::UnregisterWorkerRequest {
                worker_session_id: session,
            },
            "calc-token",
        ))
        .await
        .unwrap();

    let results: Vec<serde_json::Value> =
        tokio::task::spawn_blocking(move || ids.iter().map(|id| execution(&http, id)).collect())
            .await
            .unwrap();
    let statuses: Vec<&str> = results
        .iter()
        .map(|s| s["status"].as_str().unwrap())
        .collect();
    assert!(
        statuses.contains(&"COMPLETED") && statuses.contains(&"FAILED"),
        "{statuses:?}"
    );
    let ok = results.iter().find(|s| s["status"] == "COMPLETED").unwrap();
    let n = ok["input"]["n"].as_i64().unwrap();
    assert_eq!(
        ok["output"],
        json!({ "squared": n * n, "label": format!("{n} squared") })
    );
    let bad = results.iter().find(|s| s["status"] == "FAILED").unwrap();
    assert_eq!(bad["error"]["code"], "TOO_BIG");
}

#[tokio::test(flavor = "multi_thread")]
async fn grpc_enforces_credentials() {
    let (_http, grpc, _) = tokio::task::spawn_blocking(start).await.unwrap();
    let mut client = WorkerServiceClient::connect(grpc).await.unwrap();
    let req = |id: &str, tasks: &[&str]| pb::RegisterWorkerRequest {
        worker_id: id.into(),
        runtime: "rust".into(),
        contracts: contracts(tasks),
        sdk: String::new(),
    };

    let e = client
        .register_worker(Request::new(req("calc-1", &["calc.square"])))
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::Unauthenticated);

    let e = client
        .register_worker(authed(req("someone-else", &["calc.square"]), "calc-token"))
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::PermissionDenied);
    assert_eq!(e.metadata().get("orbit-error-code").unwrap(), "FORBIDDEN");

    let e = client
        .register_worker(authed(req("calc-1", &["payments.charge"]), "calc-token"))
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::PermissionDenied);
    assert!(
        e.message().contains("may not serve: payments.charge"),
        "{}",
        e.message()
    );

    // The shared worker token works; unknown sessions map to NOT_FOUND.
    let ok = client
        .register_worker(authed(req("anything", &["calc.square"]), "shared-worker"))
        .await
        .unwrap();
    assert!(ok.into_inner().accepted);
    let e = client
        .heartbeat(authed(
            pb::HeartbeatRequest {
                worker_session_id: "nope".into(),
            },
            "shared-worker",
        ))
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::NotFound);
    assert!(
        e.message().starts_with("UNKNOWN_SESSION"),
        "{}",
        e.message()
    );
}

#[test]
fn proto_copy_matches_the_canonical_protocol() {
    let root = env!("CARGO_MANIFEST_DIR");
    let canonical = std::fs::read_to_string(format!(
        "{root}/../../protocol/orbit/worker/v1/worker.proto"
    ))
    .unwrap();
    let copy = std::fs::read_to_string(format!("{root}/proto/worker.proto")).unwrap();
    assert_eq!(
        canonical, copy,
        "copy protocol/orbit/worker/v1/worker.proto to crates/orbit-runtime/proto/"
    );
}

/// Generate a CA, a server certificate (127.0.0.1/localhost) and a client
/// certificate for `client` with openssl. `None` if openssl is unavailable.
fn pki(client: &str) -> Option<std::path::PathBuf> {
    let dir = std::env::temp_dir().join(format!("orbit-grpc-pki-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).ok()?;
    let run = |args: &[&str]| {
        std::process::Command::new("openssl")
            .args(args)
            .current_dir(&dir)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    };
    let ok = run(&[
        "req",
        "-x509",
        "-newkey",
        "rsa:2048",
        "-nodes",
        "-keyout",
        "ca-key.pem",
        "-out",
        "ca.pem",
        "-days",
        "2",
        "-subj",
        "/CN=gRPC test CA",
        "-addext",
        "basicConstraints=critical,CA:TRUE",
        "-addext",
        "keyUsage=critical,keyCertSign,cRLSign",
    ]);
    if !ok {
        return None;
    }
    for (name, ext) in [
        (
            "server",
            "subjectAltName=DNS:localhost,IP:127.0.0.1\nextendedKeyUsage=serverAuth\n".to_string(),
        ),
        (
            "client",
            format!("subjectAltName=DNS:{client}\nextendedKeyUsage=clientAuth\n"),
        ),
    ] {
        let cn = if name == "server" {
            "localhost"
        } else {
            client
        };
        std::fs::write(dir.join(format!("{name}.cnf")), ext).ok()?;
        assert!(run(&[
            "req",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-keyout",
            &format!("{name}-key.pem"),
            "-out",
            &format!("{name}.csr"),
            "-subj",
            &format!("/CN={cn}")
        ]));
        assert!(run(&[
            "x509",
            "-req",
            "-in",
            &format!("{name}.csr"),
            "-CA",
            "ca.pem",
            "-CAkey",
            "ca-key.pem",
            "-CAcreateserial",
            "-out",
            &format!("{name}.pem"),
            "-days",
            "2",
            "-extfile",
            &format!("{name}.cnf")
        ]));
    }
    Some(dir)
}

#[tokio::test(flavor = "multi_thread")]
async fn grpc_over_mutual_tls() {
    // Two rustls crypto backends are compiled into this binary; a TLS client
    // must choose one explicitly (the server does the same in `serve`).
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let Some(dir) = pki("calc-mtls") else {
        eprintln!("skipping: openssl not available");
        return;
    };
    let ((http_l, _), (grpc_l, grpc)) = (listen(), listen());
    let identities = Identities::parse(
        "[[worker]]\nid = \"calc-mtls\"\nclient_cert = \"calc-mtls\"\ntasks = [\"calc.*\"]\n",
    )
    .unwrap();
    let tls = orbit_runtime::server::TlsConfig {
        cert: dir.join("server.pem"),
        key: dir.join("server-key.pem"),
        client_ca: Some(dir.join("ca.pem")),
    };
    std::thread::spawn(move || {
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(async move {
                let engine = Engine::open(
                    Store::in_memory().unwrap(),
                    EngineConfig::default(),
                    system_clock(),
                )
                .unwrap();
                let cfg = ServerConfig {
                    addr: "127.0.0.1:0".parse().unwrap(),
                    auth_token: Some("admin".into()),
                    worker_token: None,
                    identities,
                    tls: Some(tls),
                    grpc_addr: None,
                    listener: Some(http_l),
                    grpc_listener: Some(grpc_l),
                    body_limit: 1 << 20,
                    tick_interval: Duration::from_millis(20),
                };
                serve(engine, cfg, std::future::pending()).await.unwrap();
            });
    });
    for _ in 0..200 {
        if std::net::TcpStream::connect(("127.0.0.1", grpc)).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let read = |f: &str| std::fs::read(dir.join(f)).unwrap();
    let ca = tonic::transport::Certificate::from_pem(read("ca.pem"));
    let connect = |identity: Option<tonic::transport::Identity>| {
        let mut tls = tonic::transport::ClientTlsConfig::new()
            .ca_certificate(ca.clone())
            .domain_name("localhost");
        if let Some(i) = identity {
            tls = tls.identity(i);
        }
        tonic::transport::Channel::from_shared(format!("https://127.0.0.1:{grpc}"))
            .unwrap()
            .tls_config(tls)
            .unwrap()
    };
    let register = pb::RegisterWorkerRequest {
        worker_id: "calc-mtls".into(),
        runtime: "rust".into(),
        contracts: contracts(&["calc.square"]),
        sdk: String::new(),
    };
    // With the client certificate: identified as `calc-mtls`, no token needed.
    let channel = connect(Some(tonic::transport::Identity::from_pem(
        read("client.pem"),
        read("client-key.pem"),
    )))
    .connect()
    .await
    .unwrap();
    let ok = WorkerServiceClient::new(channel)
        .register_worker(Request::new(register.clone()))
        .await
        .unwrap();
    assert!(ok.into_inner().accepted);
    // Without a certificate or token: unauthenticated.
    let channel = connect(None).connect().await.unwrap();
    let e = WorkerServiceClient::new(channel)
        .register_worker(Request::new(register))
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::Unauthenticated);
}
