//! The built-in operations UI (`/ui/`).
//!
//! Static HTML, CSS and JavaScript compiled into the binary. The assets hold
//! no data and no secrets, so they are served without authentication; the
//! page asks for a token and calls the HTTP API (`/v1/*`) with it, so the
//! API's own authentication and authorization decide what each caller sees.
//!
//! The router is merged outside the API's authentication and cluster layers:
//! a standby or shared-mode node serves the UI itself, and its API answers
//! the reads exactly as it would for any other client.

use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;

const INDEX_HTML: &str = include_str!("../ui/index.html");
const APP_JS: &str = include_str!("../ui/app.js");
const APP_CSS: &str = include_str!("../ui/app.css");
const FAVICON_SVG: &str = include_str!("../ui/favicon.svg");

/// Content-Security-Policy of every UI response. Scripts and styles come only
/// from the separate same-origin assets (no inline script or style), the page
/// talks only to its own origin, and it cannot be framed.
pub const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; \
img-src 'self' data:; connect-src 'self'; font-src 'self'; base-uri 'none'; \
form-action 'none'; frame-ancestors 'none'";

/// Routes of the UI: `/ui/` and its assets, and `/` (a redirect to `/ui/`
/// for browsers).
pub fn router() -> Router {
    Router::new()
        .route("/", get(root))
        .route("/ui", get(|| async { redirect("/ui/") }))
        .route(
            "/ui/",
            get(|| async { asset("text/html; charset=utf-8", INDEX_HTML) }),
        )
        .route(
            "/ui/index.html",
            get(|| async { asset("text/html; charset=utf-8", INDEX_HTML) }),
        )
        .route(
            "/ui/app.js",
            get(|| async { asset("text/javascript; charset=utf-8", APP_JS) }),
        )
        .route(
            "/ui/app.css",
            get(|| async { asset("text/css; charset=utf-8", APP_CSS) }),
        )
        .route(
            "/ui/favicon.svg",
            get(|| async { asset("image/svg+xml", FAVICON_SVG) }),
        )
        .route("/ui/{*rest}", get(not_found))
}

fn security_headers(headers: &mut HeaderMap) {
    let set = |h: &mut HeaderMap, k: &'static str, v: &'static str| {
        h.insert(k, HeaderValue::from_static(v));
    };
    set(headers, "content-security-policy", CSP);
    set(headers, "x-content-type-options", "nosniff");
    set(headers, "x-frame-options", "DENY");
    set(headers, "referrer-policy", "no-referrer");
    set(headers, "cross-origin-opener-policy", "same-origin");
    set(headers, "cross-origin-resource-policy", "same-origin");
    set(
        headers,
        "permissions-policy",
        "camera=(), microphone=(), geolocation=(), payment=()",
    );
    // The assets change with the binary; revalidate rather than cache stale copies.
    set(headers, "cache-control", "no-cache");
}

fn asset(content_type: &'static str, body: &'static str) -> Response {
    let mut resp = (StatusCode::OK, body).into_response();
    resp.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    security_headers(resp.headers_mut());
    resp
}

fn redirect(to: &'static str) -> Response {
    let mut resp = StatusCode::FOUND.into_response();
    resp.headers_mut()
        .insert(header::LOCATION, HeaderValue::from_static(to));
    security_headers(resp.headers_mut());
    resp
}

async fn not_found() -> Response {
    let mut resp = (StatusCode::NOT_FOUND, "not found\n").into_response();
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    security_headers(resp.headers_mut());
    resp
}

/// `GET /`: browsers (`Accept: text/html`) are sent to the UI; API clients
/// get the same JSON 404 as any other unknown path.
async fn root(headers: HeaderMap) -> Response {
    let html = headers
        .get_all(header::ACCEPT)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .any(|v| v.contains("text/html"));
    if html {
        return redirect("/ui/");
    }
    (
        StatusCode::NOT_FOUND,
        axum::Json(serde_json::json!({ "error": {
            "code": "NOT_FOUND",
            "message": "no such endpoint; the API is under /v1/ and the web UI under /ui/",
        }})),
    )
        .into_response()
}
