//! JSON Schema (draft 2020-12) for ORBIT types, so that other systems can
//! validate payloads before sending them to the runtime or a worker.
//!
//! The schemas follow the runtime's validation rules in `orbit_ir::types`:
//!
//! | ORBIT type | Schema |
//! |---|---|
//! | `String` | `{"type": "string"}` |
//! | `Int` | `{"type": "integer"}` with the signed 64-bit range |
//! | `Float` | `{"type": "number"}` |
//! | `Bool` | `{"type": "boolean"}` |
//! | `Bytes` | string, `contentEncoding: base64` |
//! | `Duration` | integer milliseconds, minimum 0 |
//! | `Timestamp` | string, `format: date-time` |
//! | `UUID` | string, `format: uuid` |
//! | `JSON` | `true` (any value) |
//! | `List<T>` | array of `T` |
//! | `Map<String, T>` | object whose values are `T` |
//! | `T?` | `anyOf: [T, null]` |
//! | record | object with `properties`, `required` and `default`s |
//!
//! Workflow inputs are validated strictly by the runtime (unknown fields are
//! rejected), so their records carry `additionalProperties: false`. Signal
//! payloads, task inputs and outputs, workflow outputs and query results are
//! validated leniently and allow extra fields. `format` is an annotation in
//! draft 2020-12; validators check it only when asked to. The runtime's own
//! checks for base64 and timestamps are stricter than any schema.

use orbit_ir::{TaskContract, Type, WorkflowIr, WORKFLOW_CONTRACT_PREFIX};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

pub const DRAFT: &str = "https://json-schema.org/draft/2020-12/schema";

/// Schema for a type. `strict` closes records (`additionalProperties: false`).
pub fn type_schema(t: &Type, strict: bool) -> Value {
    match t {
        Type::String => json!({ "type": "string" }),
        Type::Int => json!({ "type": "integer", "minimum": i64::MIN, "maximum": i64::MAX }),
        Type::Float => json!({ "type": "number" }),
        Type::Bool => json!({ "type": "boolean" }),
        Type::Bytes => json!({ "type": "string", "contentEncoding": "base64" }),
        Type::Duration => {
            json!({ "type": "integer", "minimum": 0, "description": "duration in milliseconds" })
        }
        Type::Timestamp => json!({ "type": "string", "format": "date-time" }),
        Type::Uuid => json!({ "type": "string", "format": "uuid" }),
        Type::Json => json!(true),
        Type::Null => json!({ "type": "null" }),
        Type::List { item } => json!({ "type": "array", "items": type_schema(item, strict) }),
        Type::Map { value, .. } => {
            json!({ "type": "object", "additionalProperties": type_schema(value, strict) })
        }
        Type::Optional { inner } => {
            json!({ "anyOf": [type_schema(inner, strict), { "type": "null" }] })
        }
        Type::Record { name, fields } => {
            let mut props = Map::new();
            let mut required = vec![];
            for f in fields {
                let mut s = type_schema(&f.ty, strict);
                if let (Some(d), Value::Object(m)) = (&f.default, &mut s) {
                    m.insert("default".into(), d.clone());
                }
                if let (Some(d), Value::Bool(true)) = (&f.default, &s) {
                    s = json!({ "default": d });
                }
                props.insert(f.name.clone(), s);
                if f.required() {
                    required.push(json!(f.name));
                }
            }
            let mut m = Map::new();
            if let Some(n) = name {
                m.insert("title".into(), json!(n));
            }
            m.insert("type".into(), json!("object"));
            m.insert("properties".into(), Value::Object(props));
            if !required.is_empty() {
                m.insert("required".into(), Value::Array(required));
            }
            if strict {
                m.insert("additionalProperties".into(), json!(false));
            }
            Value::Object(m)
        }
    }
}

fn document(id: &str, title: String, t: &Type, strict: bool) -> Value {
    let mut s = type_schema(t, strict);
    let body = match &mut s {
        Value::Object(m) => std::mem::take(m),
        _ => Map::new(),
    };
    let mut m = Map::new();
    m.insert("$schema".into(), json!(DRAFT));
    m.insert("$id".into(), json!(id));
    m.insert("title".into(), json!(title));
    for (k, v) in body {
        if k != "title" {
            m.insert(k, v);
        }
    }
    Value::Object(m)
}

/// Every schema of a workflow, keyed by a file-friendly name such as
/// `workflow.Checkout.input`, `workflow.Checkout.signal.review`,
/// `workflow.Checkout.query.status`.
pub fn workflow_schemas(wf: &WorkflowIr) -> BTreeMap<String, Value> {
    let mut out = BTreeMap::new();
    let base = format!("workflow.{}", wf.name);
    let mut add = |suffix: &str, title: String, t: &Type, strict: bool| {
        let key = format!("{base}.{suffix}");
        let id = format!("urn:orbit:{key}@{}", wf.version);
        out.insert(key, document(&id, title, t, strict));
    };
    add(
        "input",
        format!("{}@{} input", wf.name, wf.version),
        &wf.input,
        true,
    );
    add(
        "output",
        format!("{}@{} output", wf.name, wf.version),
        &wf.output,
        false,
    );
    for (sig, t) in &wf.signals {
        add(
            &format!("signal.{sig}"),
            format!("{}@{} signal {sig}", wf.name, wf.version),
            t,
            false,
        );
    }
    for (q, ir) in &wf.queries {
        add(
            &format!("query.{q}"),
            format!("{}@{} query {q}", wf.name, wf.version),
            &ir.output_type,
            false,
        );
    }
    out
}

/// Input and output schemas of a task contract, keyed
/// `task.payments.charge.input` and `task.payments.charge.output`.
pub fn task_schemas(c: &TaskContract) -> BTreeMap<String, Value> {
    let mut out = BTreeMap::new();
    for (part, t) in [("input", &c.input), ("output", &c.output)] {
        let key = format!("task.{}.{part}", c.name);
        let id = format!("urn:orbit:{key}@{}", c.version);
        out.insert(
            key,
            document(&id, format!("{}@{} {part}", c.name, c.version), t, false),
        );
    }
    out
}

/// All schemas for a set of workflows and contracts. Child-workflow contracts
/// are covered by the child's own workflow schemas.
pub fn all_schemas<'a>(
    workflows: &[WorkflowIr],
    contracts: impl IntoIterator<Item = &'a TaskContract>,
) -> BTreeMap<String, Value> {
    let mut out = BTreeMap::new();
    for wf in workflows {
        out.extend(workflow_schemas(wf));
    }
    for c in contracts {
        if !c.name.starts_with(WORKFLOW_CONTRACT_PREFIX) {
            out.extend(task_schemas(c));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile_str;

    #[test]
    fn records_optional_defaults_and_strictness() {
        let c = compile_str(
            "s.orbit",
            r#"
schema Item { sku: String, quantity: Int }
workflow W {
    version "1.0.0"
    input { id: UUID, at: Timestamp?, currency: String = "USD", items: List<Item>, tags: Map<String, Int>, raw: JSON }
    signal go { ok: Bool }
    step wait { await go timeout 1d }
    query q = wait.ok
}
"#,
        );
        assert!(!c.has_errors(), "{}", c.render_diagnostics(false));
        let s = workflow_schemas(&c.workflows[0]);
        let input = &s["workflow.W.input"];
        assert_eq!(input["$schema"], DRAFT);
        assert_eq!(input["additionalProperties"], false);
        assert_eq!(input["required"], json!(["id", "items", "tags"]));
        assert_eq!(input["properties"]["id"]["format"], "uuid");
        assert_eq!(
            input["properties"]["at"]["anyOf"][1],
            json!({ "type": "null" })
        );
        assert_eq!(input["properties"]["currency"]["default"], "USD");
        assert_eq!(input["properties"]["items"]["items"]["title"], "Item");
        assert_eq!(
            input["properties"]["items"]["items"]["additionalProperties"],
            false
        );
        assert_eq!(input["properties"]["raw"], json!(true));
        let sig = &s["workflow.W.signal.go"];
        assert!(sig.get("additionalProperties").is_none());
        assert_eq!(s["workflow.W.query.q"]["anyOf"][0]["type"], "boolean");
    }
}
