//! Minimal blocking client for the ORBIT runtime API.

use serde_json::Value;
use std::time::Duration;

pub struct Client {
    base: String,
    token: Option<String>,
    agent: ureq::Agent,
}

#[derive(Debug)]
pub struct ApiError {
    pub status: u16,
    pub code: String,
    pub message: String,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.status == 0 {
            write!(f, "{}", self.message)
        } else {
            write!(f, "{} ({}): {}", self.code, self.status, self.message)
        }
    }
}

pub type Result<T> = std::result::Result<T, ApiError>;

impl Client {
    pub fn new(base: &str, token: Option<String>, ca_pem: Option<Vec<u8>>) -> Self {
        let mut config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(330)));
        if let Some(pem) = ca_pem {
            use ureq::tls::{Certificate, RootCerts, TlsConfig};
            let certs: Vec<Certificate<'static>> = ureq::tls::parse_pem(&pem)
                .filter_map(|item| match item {
                    Ok(ureq::tls::PemItem::Certificate(c)) => Some(c),
                    _ => None,
                })
                .collect();
            config = config.tls_config(
                TlsConfig::builder()
                    .root_certs(RootCerts::new_with_certs(&certs))
                    .build(),
            );
        }
        let agent: ureq::Agent = config.build().into();
        Client {
            base: base.trim_end_matches('/').to_string(),
            token,
            agent,
        }
    }

    fn handle(
        &self,
        r: std::result::Result<ureq::http::Response<ureq::Body>, ureq::Error>,
    ) -> Result<(u16, Value)> {
        let mut resp = r.map_err(|e| ApiError {
            status: 0,
            code: "CONNECTION".into(),
            message: format!("cannot reach the ORBIT runtime at {}: {e}\n  hint: start it with `orbit server` or set --server / ORBIT_SERVER", self.base),
        })?;
        let status = resp.status().as_u16();
        let text = resp.body_mut().read_to_string().unwrap_or_default();
        let body: Value = if text.is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&text).unwrap_or(Value::String(text))
        };
        if status >= 400 {
            let err = &body["error"];
            return Err(ApiError {
                status,
                code: err["code"].as_str().unwrap_or("HTTP_ERROR").to_string(),
                message: err["message"]
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| body.to_string()),
            });
        }
        Ok((status, body))
    }

    pub fn get(&self, path: &str) -> Result<Value> {
        let mut req = self.agent.get(format!("{}{path}", self.base));
        if let Some(t) = &self.token {
            req = req.header("authorization", format!("Bearer {t}"));
        }
        self.handle(req.call()).map(|r| r.1)
    }

    pub fn post(&self, path: &str, body: &Value) -> Result<(u16, Value)> {
        let mut req = self.agent.post(format!("{}{path}", self.base));
        if let Some(t) = &self.token {
            req = req.header("authorization", format!("Bearer {t}"));
        }
        self.handle(req.send_json(body))
    }

    pub fn post_raw(&self, path: &str, body: String) -> Result<(u16, Value)> {
        let mut req = self
            .agent
            .post(format!("{}{path}", self.base))
            .header("content-type", "application/json");
        if let Some(t) = &self.token {
            req = req.header("authorization", format!("Bearer {t}"));
        }
        self.handle(req.send(body))
    }
}
