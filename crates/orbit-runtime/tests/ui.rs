//! The built-in web UI against a real server: assets, content types,
//! security headers, authentication boundaries, `/` redirect and `--no-ui`.

use orbit_runtime::engine::system_clock;
use orbit_runtime::server::{serve, ServerConfig};
use orbit_runtime::{Engine, EngineConfig, Store};
use std::time::Duration;

const ADMIN: &str = "ui-test-admin-secret";
const BEARER: &str = "Bearer ui-test-admin-secret";

/// Start a server with an admin token; returns its base URL.
fn start(ui: bool) -> String {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let engine = Engine::open(
        Store::in_memory().unwrap(),
        EngineConfig::default(),
        system_clock(),
    )
    .unwrap();
    std::thread::spawn(move || {
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(async move {
                let cfg = ServerConfig {
                    addr: format!("127.0.0.1:{port}").parse().unwrap(),
                    auth_token: Some(ADMIN.into()),
                    worker_token: None,
                    identities: Default::default(),
                    tls: None,
                    grpc_addr: None,
                    listener: Some(l),
                    grpc_listener: None,
                    body_limit: 1 << 20,
                    tick_interval: Duration::from_millis(20),
                    ui,
                };
                serve(engine, cfg, std::future::pending()).await.unwrap();
            });
    });
    let url = format!("http://127.0.0.1:{port}");
    for _ in 0..200 {
        if ureq::get(format!("{url}/v1/health")).call().is_ok() {
            return url;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("server did not start");
}

struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

fn get(url: &str, headers: &[(&str, &str)]) -> Reply {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .max_redirects_will_error(false)
        .timeout_global(Some(Duration::from_secs(10)))
        .build()
        .into();
    let mut req = agent.get(url);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let mut resp = req.call().unwrap();
    let headers = resp
        .headers()
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();
    Reply {
        status: resp.status().as_u16(),
        headers,
        body: resp.body_mut().read_to_string().unwrap_or_default(),
    }
}

fn assert_security_headers(r: &Reply) {
    let csp = r.header("content-security-policy").expect("CSP header");
    assert!(csp.contains("default-src 'none'"), "{csp}");
    assert!(csp.contains("script-src 'self'"), "{csp}");
    assert!(csp.contains("frame-ancestors 'none'"), "{csp}");
    assert!(!csp.contains("unsafe-inline"), "{csp}");
    assert!(!csp.contains("unsafe-eval"), "{csp}");
    assert_eq!(r.header("x-content-type-options"), Some("nosniff"));
    assert_eq!(r.header("referrer-policy"), Some("no-referrer"));
    assert_eq!(r.header("x-frame-options"), Some("DENY"));
}

#[test]
fn ui_assets_are_served_without_credentials_with_security_headers() {
    let url = start(true);
    for (path, ctype) in [
        ("/ui/", "text/html; charset=utf-8"),
        ("/ui/index.html", "text/html; charset=utf-8"),
        ("/ui/app.js", "text/javascript; charset=utf-8"),
        ("/ui/app.css", "text/css; charset=utf-8"),
        ("/ui/favicon.svg", "image/svg+xml"),
    ] {
        let r = get(&format!("{url}{path}"), &[]);
        assert_eq!(r.status, 200, "{path}");
        assert_eq!(r.header("content-type"), Some(ctype), "{path}");
        assert!(!r.body.is_empty(), "{path}");
        assert_security_headers(&r);
    }
    let index = get(&format!("{url}/ui/"), &[]).body;
    // No inline script or style: everything comes from the separate assets.
    assert!(index.contains(r#"<script src="app.js" defer></script>"#));
    assert!(!index.contains("<style"));
    assert_eq!(index.matches("<script").count(), 1);
    // The assets embed no secret.
    for path in ["/ui/", "/ui/app.js", "/ui/app.css"] {
        assert!(!get(&format!("{url}{path}"), &[]).body.contains(ADMIN));
    }
    let js = get(&format!("{url}/ui/app.js"), &[]).body;
    assert!(!js.contains("innerHTML"), "the UI must not use innerHTML");
    assert!(js.contains("sessionStorage"));
    assert!(!js.contains("localStorage"));

    // Unknown assets: a plain 404, still with the headers.
    let r = get(&format!("{url}/ui/nope.js"), &[]);
    assert_eq!(r.status, 404);
    assert_security_headers(&r);
    // `/ui` without the slash goes to `/ui/`.
    let r = get(&format!("{url}/ui"), &[]);
    assert_eq!(r.status, 302);
    assert_eq!(r.header("location"), Some("/ui/"));
}

#[test]
fn the_api_still_requires_credentials() {
    let url = start(true);
    let r = get(&format!("{url}/v1/executions"), &[]);
    assert_eq!(r.status, 401);
    assert!(r.body.contains("UNAUTHENTICATED"));
    assert!(r.header("content-security-policy").is_none());
    let r = get(
        &format!("{url}/v1/workflows"),
        &[("authorization", "Bearer wrong")],
    );
    assert_eq!(r.status, 401);
    let r = get(
        &format!("{url}/v1/executions"),
        &[("authorization", BEARER)],
    );
    assert_eq!(r.status, 200);
    assert_eq!(r.header("x-orbit-api-version"), Some("orbit-api/1"));
    // Health stays open, as before.
    assert_eq!(get(&format!("{url}/v1/health"), &[]).status, 200);
}

#[test]
fn root_redirects_browsers_to_the_ui() {
    let url = start(true);
    let browser = get(
        &format!("{url}/"),
        &[(
            "accept",
            "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
        )],
    );
    assert_eq!(browser.status, 302);
    assert_eq!(browser.header("location"), Some("/ui/"));
    // API clients get a JSON 404 rather than a redirect to HTML.
    for accept in ["application/json", "*/*"] {
        let r = get(&format!("{url}/"), &[("accept", accept)]);
        assert_eq!(r.status, 404, "{accept}");
        assert!(r.body.contains("NOT_FOUND"), "{}", r.body);
        assert!(r.header("location").is_none());
    }
}

#[test]
fn no_ui_disables_the_ui() {
    let url = start(false);
    for path in ["/ui/", "/ui/app.js", "/ui/app.css"] {
        let r = get(&format!("{url}{path}"), &[]);
        assert_ne!(r.status, 200, "{path}");
        assert!(r.header("content-security-policy").is_none(), "{path}");
        assert!(!r.body.contains("<html"), "{path}");
    }
    // `/` does not redirect to a UI that is not there.
    let r = get(&format!("{url}/"), &[("accept", "text/html")]);
    assert_ne!(r.status, 302);
    assert!(r.header("location").is_none());
    let r = get(
        &format!("{url}/v1/executions"),
        &[("authorization", BEARER)],
    );
    assert_eq!(r.status, 200);
}
