//! Built-in `orbit.http.request` task: lets a workflow call an HTTP endpoint
//! without writing a worker. Disabled unless the server is started with
//! `--enable-http-adapter`, because it lets workflow authors make outbound
//! requests from the runtime host.

use crate::model::{codes, TaskError};
use serde_json::{json, Map, Value};
use std::time::Duration;

pub fn execute(input: &Value, timeout_ms: Option<u64>) -> Result<Value, TaskError> {
    let method = input
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or("GET")
        .to_uppercase();
    let url = input
        .get("url")
        .and_then(Value::as_str)
        .ok_or_else(|| TaskError::new(codes::HTTP_ERROR, "missing url", false))?;
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(TaskError::new(
            codes::HTTP_ERROR,
            "only http:// and https:// URLs are allowed",
            false,
        ));
    }
    let config = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_millis(timeout_ms.unwrap_or(30_000))))
        .build();
    let agent: ureq::Agent = config.into();
    let headers: Vec<(String, String)> = input
        .get("headers")
        .and_then(Value::as_object)
        .map(|h| {
            h.iter()
                .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string())))
                .collect()
        })
        .unwrap_or_default();
    let body = input.get("body").cloned().unwrap_or(Value::Null);

    let result = match method.as_str() {
        "GET" | "DELETE" | "HEAD" | "OPTIONS" => {
            let mut req = match method.as_str() {
                "GET" => agent.get(url),
                "DELETE" => agent.delete(url),
                "HEAD" => agent.head(url),
                _ => agent.options(url),
            };
            for (k, v) in &headers {
                req = req.header(k, v);
            }
            req.call()
        }
        "POST" | "PUT" | "PATCH" => {
            let mut req = match method.as_str() {
                "POST" => agent.post(url),
                "PUT" => agent.put(url),
                _ => agent.patch(url),
            };
            for (k, v) in &headers {
                req = req.header(k, v);
            }
            match &body {
                Value::Null => req.send_empty(),
                Value::String(s) => req.send(s.as_str()),
                other => req.send_json(other),
            }
        }
        other => {
            return Err(TaskError::new(
                codes::HTTP_ERROR,
                format!("unsupported method {other}"),
                false,
            ))
        }
    };

    let mut resp = result
        .map_err(|e| TaskError::new(codes::HTTP_ERROR, format!("request failed: {e}"), true))?;
    let status = resp.status().as_u16();
    let mut out_headers = Map::new();
    for (k, v) in resp.headers() {
        if let Ok(v) = v.to_str() {
            out_headers.insert(k.as_str().to_string(), Value::String(v.to_string()));
        }
    }
    let text = resp
        .body_mut()
        .with_config()
        .limit(8 * 1024 * 1024)
        .read_to_string()
        .unwrap_or_default();
    let body = serde_json::from_str::<Value>(&text).unwrap_or(Value::String(text));
    if status >= 500 || status == 429 {
        let mut e = TaskError::new(
            codes::HTTP_ERROR,
            format!("server responded {status}"),
            true,
        );
        e.details = Some(json!({ "status": status, "body": body }));
        return Err(e);
    }
    Ok(json!({ "status": status, "headers": out_headers, "body": body }))
}
