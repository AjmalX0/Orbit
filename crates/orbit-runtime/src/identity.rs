//! Per-worker identities.
//!
//! A workers file (`orbit server --workers-file workers.toml`) declares every
//! worker that may connect, how it authenticates, and what it may do:
//!
//! ```toml
//! [[worker]]
//! id = "payments-1"
//! # SHA-256 of the bearer token (hex). Generate with `orbit token`.
//! token_sha256 = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"
//! tasks = ["payments.*"]        # task patterns this worker may serve
//! runtime = "python"            # optional: must match the announced runtime
//!
//! [[worker]]
//! id = "inventory"
//! # Authenticate with a client certificate (mutual TLS) instead of a token:
//! # matched against the certificate's DNS SANs and subject common name.
//! client_cert = "inventory.workers.internal"
//! tasks = ["inventory.reserve", "inventory.release"]
//! ```
//!
//! A worker can register only under its own `id`, announce only tasks its
//! patterns allow, and use only its own sessions and task tokens.

use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerIdentity {
    pub id: String,
    /// Hex SHA-256 of the worker's bearer token.
    #[serde(default)]
    pub token_sha256: Option<String>,
    /// Certificate name (DNS SAN or subject CN) for mutual-TLS authentication.
    #[serde(default)]
    pub client_cert: Option<String>,
    /// Task name patterns: exact names, `prefix.*`, or `*`.
    pub tasks: Vec<String>,
    #[serde(default)]
    pub runtime: Option<String>,
}

impl WorkerIdentity {
    pub fn may_serve(&self, task: &str) -> bool {
        self.tasks.iter().any(|p| pattern_matches(p, task))
    }
}

pub fn pattern_matches(pattern: &str, task: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => task.starts_with(prefix),
        None => pattern == task,
    }
}

/// Actions a client identity can be granted.
pub const CLIENT_ACTIONS: &[&str] = &["start", "signal", "query", "read", "cancel"];

fn default_client_actions() -> Vec<String> {
    ["start", "signal", "query", "read"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// An application allowed to use the execution API for some workflows only
/// (`[[client]]` in the workers file).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientIdentity {
    pub id: String,
    #[serde(default)]
    pub token_sha256: Option<String>,
    #[serde(default)]
    pub client_cert: Option<String>,
    /// Workflow name patterns: exact names, `Prefix*`, or `*`.
    pub workflows: Vec<String>,
    /// Granted actions; defaults to start, signal, query and read.
    #[serde(default = "default_client_actions")]
    pub actions: Vec<String>,
}

impl ClientIdentity {
    pub fn allows(&self, action: &str, workflow: &str) -> bool {
        self.actions.iter().any(|a| a == action) && self.workflows.iter().any(|p| glob(p, workflow))
    }

    /// May this client see anything about the workflow at all?
    pub fn can_read(&self, workflow: &str) -> bool {
        self.allows("read", workflow)
    }
}

/// `*` matches anything; `Prefix*` a prefix; otherwise an exact name.
fn glob(pattern: &str, name: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None => pattern == name,
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkersFile {
    #[serde(default, rename = "worker")]
    pub workers: Vec<WorkerIdentity>,
    #[serde(default, rename = "client")]
    pub clients: Vec<ClientIdentity>,
}

#[derive(Debug, Default)]
pub struct Identities {
    workers: Vec<WorkerIdentity>,
    clients: Vec<ClientIdentity>,
}

pub fn sha256_hex(s: &str) -> String {
    Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

impl Identities {
    pub fn load(path: &Path) -> Result<Identities, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        Self::parse(&text).map_err(|e| format!("{}: {e}", path.display()))
    }

    pub fn parse(text: &str) -> Result<Identities, String> {
        let file: WorkersFile = toml::from_str(text).map_err(|e| e.to_string())?;
        let mut seen = std::collections::HashSet::new();
        for w in &file.workers {
            if w.id.trim().is_empty() {
                return Err("a worker has an empty `id`".into());
            }
            if !seen.insert(w.id.clone()) {
                return Err(format!("worker `{}` is declared twice", w.id));
            }
            if w.token_sha256.is_none() && w.client_cert.is_none() {
                return Err(format!(
                    "worker `{}` needs `token_sha256` or `client_cert`",
                    w.id
                ));
            }
            if let Some(h) = &w.token_sha256 {
                if h.len() != 64 || !h.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return Err(format!(
                        "worker `{}`: `token_sha256` must be 64 hex characters (use `orbit token`)",
                        w.id
                    ));
                }
            }
            if w.tasks.is_empty() {
                return Err(format!(
                    "worker `{}` must list at least one task pattern",
                    w.id
                ));
            }
        }
        for c in &file.clients {
            if c.id.trim().is_empty() {
                return Err("a client has an empty `id`".into());
            }
            if !seen.insert(c.id.clone()) {
                return Err(format!("identity `{}` is declared twice", c.id));
            }
            if c.token_sha256.is_none() && c.client_cert.is_none() {
                return Err(format!(
                    "client `{}` needs `token_sha256` or `client_cert`",
                    c.id
                ));
            }
            if let Some(h) = &c.token_sha256 {
                if h.len() != 64 || !h.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return Err(format!(
                        "client `{}`: `token_sha256` must be 64 hex characters (use `orbit token`)",
                        c.id
                    ));
                }
            }
            if c.workflows.is_empty() {
                return Err(format!(
                    "client `{}` must list at least one workflow pattern",
                    c.id
                ));
            }
            if let Some(bad) = c
                .actions
                .iter()
                .find(|a| !CLIENT_ACTIONS.contains(&a.as_str()))
            {
                return Err(format!(
                    "client `{}`: unknown action `{bad}` (allowed: {})",
                    c.id,
                    CLIENT_ACTIONS.join(", ")
                ));
            }
        }
        Ok(Identities {
            workers: file.workers,
            clients: file.clients,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.workers.is_empty() && self.clients.is_empty()
    }

    /// The client whose token hashes to `token`'s hash.
    pub fn client_by_token(&self, token: &str) -> Option<&ClientIdentity> {
        let h = sha256_hex(token);
        self.clients.iter().find(|c| {
            c.token_sha256
                .as_ref()
                .is_some_and(|t| constant_time_eq(t.to_ascii_lowercase().as_bytes(), h.as_bytes()))
        })
    }

    /// The client identified by a verified DER client certificate.
    pub fn client_by_certificate(&self, der: &[u8]) -> Option<&ClientIdentity> {
        let names = certificate_names(der);
        self.clients.iter().find(|c| {
            c.client_cert
                .as_ref()
                .is_some_and(|n| names.iter().any(|x| x.eq_ignore_ascii_case(n)))
        })
    }

    pub fn len(&self) -> usize {
        self.workers.len()
    }

    /// The worker whose token hashes to `token`'s hash.
    pub fn by_token(&self, token: &str) -> Option<&WorkerIdentity> {
        let h = sha256_hex(token);
        self.workers.iter().find(|w| {
            w.token_sha256
                .as_ref()
                .is_some_and(|t| constant_time_eq(t.to_ascii_lowercase().as_bytes(), h.as_bytes()))
        })
    }

    /// The worker identified by a DER client certificate (already verified
    /// against the client CA by TLS).
    pub fn by_certificate(&self, der: &[u8]) -> Option<&WorkerIdentity> {
        let names = certificate_names(der);
        self.workers.iter().find(|w| {
            w.client_cert
                .as_ref()
                .is_some_and(|c| names.iter().any(|n| n.eq_ignore_ascii_case(c)))
        })
    }
}

/// DNS SANs and subject common names of a certificate.
pub fn certificate_names(der: &[u8]) -> Vec<String> {
    let Ok((_, cert)) = x509_parser::parse_x509_certificate(der) else {
        return vec![];
    };
    let mut names: Vec<String> = cert
        .subject()
        .iter_common_name()
        .filter_map(|cn| cn.as_str().ok().map(str::to_string))
        .collect();
    if let Ok(Some(san)) = cert.subject_alternative_name() {
        for n in &san.value.general_names {
            if let x509_parser::extensions::GeneralName::DNSName(d) = n {
                names.push(d.to_string());
            }
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_matches() {
        let token_hash = sha256_hex("s3cret");
        let ids = Identities::parse(&format!(
            r#"
            [[worker]]
            id = "pay"
            token_sha256 = "{token_hash}"
            tasks = ["payments.*", "audit.log"]
            [[worker]]
            id = "inv"
            client_cert = "inventory.internal"
            tasks = ["*"]
            "#
        ))
        .unwrap();
        let pay = ids.by_token("s3cret").unwrap();
        assert_eq!(pay.id, "pay");
        assert!(pay.may_serve("payments.charge") && pay.may_serve("audit.log"));
        assert!(!pay.may_serve("inventory.reserve") && !pay.may_serve("audit.logs"));
        assert!(ids.by_token("wrong").is_none());
        assert!(
            Identities::parse("[[worker]]\nid = \"x\"\ntasks = [\"*\"]").is_err(),
            "needs a credential"
        );
        assert!(Identities::parse(
            "[[worker]]\nid = \"x\"\ntoken_sha256 = \"abc\"\ntasks = [\"*\"]"
        )
        .is_err());
        assert!(Identities::parse(
            "[[worker]]\nid = \"x\"\nclient_cert = \"a\"\ntasks = [\"*\"]\nbogus = 1"
        )
        .is_err());
    }

    #[test]
    fn client_identities_grant_workflows_and_actions() {
        let h = sha256_hex("app-token");
        let ids = Identities::parse(&format!(
            r#"
            [[client]]
            id = "billing"
            token_sha256 = "{h}"
            workflows = ["Invoice*", "Refund"]
            [[client]]
            id = "ops"
            client_cert = "ops.internal"
            workflows = ["*"]
            actions = ["read", "cancel"]
            "#
        ))
        .unwrap();
        assert!(!ids.is_empty());
        let billing = ids.client_by_token("app-token").unwrap();
        assert!(billing.allows("start", "InvoiceMonthly") && billing.allows("signal", "Refund"));
        assert!(
            !billing.allows("cancel", "Refund"),
            "cancel is not a default action"
        );
        assert!(!billing.allows("start", "Payroll"));
        assert!(
            ids.by_token("app-token").is_none(),
            "a client is not a worker"
        );
        let bad_action = "[[client]]\nid = \"x\"\nclient_cert = \"a\"\nworkflows = [\"*\"]\nactions = [\"deploy\"]";
        assert!(Identities::parse(bad_action)
            .unwrap_err()
            .contains("unknown action"));
        let dup =
            "[[worker]]\nid = \"x\"\nclient_cert = \"a\"\ntasks = [\"*\"]\n[[client]]\nid = \"x\"\nclient_cert = \"b\"\nworkflows = [\"*\"]";
        assert!(Identities::parse(dup)
            .unwrap_err()
            .contains("declared twice"));
    }
}
