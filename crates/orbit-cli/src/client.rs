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
    /// With `NOT_LEADER` (a standby in a cluster): the leader's address.
    pub leader: Option<String>,
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
    pub fn new(
        base: &str,
        token: Option<String>,
        ca_pem: Option<Vec<u8>>,
        identity: Option<(Vec<u8>, Vec<u8>)>,
    ) -> Self {
        use ureq::tls::{Certificate, ClientCert, PemItem, PrivateKey, RootCerts, TlsConfig};
        let mut config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(330)));
        if ca_pem.is_some() || identity.is_some() {
            let mut tls = TlsConfig::builder();
            if let Some(pem) = ca_pem {
                let certs: Vec<Certificate<'static>> = ureq::tls::parse_pem(&pem)
                    .filter_map(|item| match item {
                        Ok(PemItem::Certificate(c)) => Some(c),
                        _ => None,
                    })
                    .collect();
                tls = tls.root_certs(RootCerts::new_with_certs(&certs));
            }
            if let Some((cert_pem, key_pem)) = identity {
                let chain: Vec<Certificate<'static>> = ureq::tls::parse_pem(&cert_pem)
                    .filter_map(|item| match item {
                        Ok(PemItem::Certificate(c)) => Some(c),
                        _ => None,
                    })
                    .collect();
                match PrivateKey::from_pem(&key_pem) {
                    Ok(key) => {
                        tls = tls
                            .client_cert(Some(ClientCert::new_with_certs(&chain, key.to_owned())))
                    }
                    Err(e) => {
                        eprintln!("error: invalid client key: {e}");
                        std::process::exit(2)
                    }
                }
            }
            config = config.tls_config(tls.build());
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
            leader: None,
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
                leader: err["leader_addr"].as_str().map(str::to_string),
            });
        }
        Ok((status, body))
    }

    /// Send a request to `base`, then once more to the leader if a cluster
    /// standby answered `NOT_LEADER` with the leader's address.
    fn send(
        &self,
        call: impl Fn(&str) -> std::result::Result<ureq::http::Response<ureq::Body>, ureq::Error>,
    ) -> Result<(u16, Value)> {
        match self.handle(call(&self.base)) {
            Err(ApiError {
                code,
                leader: Some(leader),
                ..
            }) if code == "NOT_LEADER" && leader.trim_end_matches('/') != self.base => {
                self.handle(call(leader.trim_end_matches('/')))
            }
            r => r,
        }
    }

    pub fn get(&self, path: &str) -> Result<Value> {
        self.send(|base| {
            let mut req = self.agent.get(format!("{base}{path}"));
            if let Some(t) = &self.token {
                req = req.header("authorization", format!("Bearer {t}"));
            }
            req.call()
        })
        .map(|r| r.1)
    }

    pub fn post(&self, path: &str, body: &Value) -> Result<(u16, Value)> {
        self.send(|base| {
            let mut req = self.agent.post(format!("{base}{path}"));
            if let Some(t) = &self.token {
                req = req.header("authorization", format!("Bearer {t}"));
            }
            req.send_json(body)
        })
    }

    pub fn post_raw(&self, path: &str, body: String) -> Result<(u16, Value)> {
        self.send(|base| {
            let mut req = self
                .agent
                .post(format!("{base}{path}"))
                .header("content-type", "application/json");
            if let Some(t) = &self.token {
                req = req.header("authorization", format!("Bearer {t}"));
            }
            req.send(body.as_str())
        })
    }
}
