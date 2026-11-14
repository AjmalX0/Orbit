//! Resolved ORBIT types and runtime payload validation.
//!
//! Values travel between the runtime and workers as JSON. The canonical JSON
//! encoding of each ORBIT type is:
//!
//! | ORBIT type  | JSON encoding                                   |
//! |-------------|-------------------------------------------------|
//! | `String`    | string                                          |
//! | `Int`       | integer (signed 64-bit)                         |
//! | `Float`     | number (integers are accepted)                  |
//! | `Bool`      | `true` / `false`                                |
//! | `Bytes`     | base64 string                                   |
//! | `Duration`  | integer milliseconds                            |
//! | `Timestamp` | RFC 3339 string in UTC, e.g. `2026-01-01T00:00:00Z` |
//! | `UUID`      | canonical hyphenated UUID string                |
//! | `JSON`      | any JSON value                                  |
//! | `List<T>`   | array                                           |
//! | `Map<String,T>` | object                                      |
//! | `T?`        | `null` or the encoding of `T`                   |
//! | record      | object                                          |

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Type {
    String,
    Int,
    Float,
    Bool,
    Bytes,
    Duration,
    Timestamp,
    Uuid,
    Json,
    /// The type of the `null` literal. Never appears in a field position.
    Null,
    List {
        item: Box<Type>,
    },
    Map {
        key: Box<Type>,
        value: Box<Type>,
    },
    Optional {
        inner: Box<Type>,
    },
    Record {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        fields: Vec<FieldType>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FieldType {
    pub name: String,
    #[serde(rename = "type")]
    pub ty: Type,
    /// `Some(Value::Null)` is a real default of `null`, distinct from no default.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub default: Option<Value>,
}

/// Deserialize a present field as `Some`, even when its value is `null`.
fn present<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(d).map(Some)
}

impl FieldType {
    /// A field must be supplied unless it is optional or has a default.
    pub fn required(&self) -> bool {
        self.default.is_none() && !self.ty.is_optional()
    }
}

impl Type {
    pub fn optional(self) -> Type {
        match self {
            Type::Optional { .. } | Type::Json | Type::Null => self,
            other => Type::Optional {
                inner: Box::new(other),
            },
        }
    }

    pub fn is_optional(&self) -> bool {
        matches!(self, Type::Optional { .. } | Type::Json | Type::Null)
    }

    /// Strip one level of `?`.
    pub fn unwrap_optional(&self) -> &Type {
        match self {
            Type::Optional { inner } => inner,
            other => other,
        }
    }

    pub fn is_numeric(&self) -> bool {
        matches!(self, Type::Int | Type::Float)
    }

    pub fn record(name: Option<&str>, fields: Vec<FieldType>) -> Type {
        Type::Record {
            name: name.map(str::to_string),
            fields,
        }
    }

    pub fn field(&self, name: &str) -> Option<&FieldType> {
        match self {
            Type::Record { fields, .. } => fields.iter().find(|f| f.name == name),
            _ => None,
        }
    }

    /// Validate a JSON value against this type.
    ///
    /// With `strict`, records reject fields they do not declare. Execution
    /// inputs are validated strictly; worker outputs are validated leniently so
    /// that workers can add fields without breaking older workflows.
    pub fn validate(&self, value: &Value, strict: bool) -> Result<(), ValidationError> {
        let mut path = String::from("$");
        self.validate_at(value, strict, &mut path)
    }

    fn validate_at(
        &self,
        value: &Value,
        strict: bool,
        path: &mut String,
    ) -> Result<(), ValidationError> {
        let fail = |path: &str, expected: &Type| {
            Err(ValidationError {
                path: path.to_string(),
                message: format!("expected {expected}, found {}", describe(value)),
            })
        };
        match self {
            Type::Json => Ok(()),
            Type::Null => {
                if value.is_null() {
                    Ok(())
                } else {
                    fail(path, self)
                }
            }
            Type::Optional { inner } => {
                if value.is_null() {
                    Ok(())
                } else {
                    inner.validate_at(value, strict, path)
                }
            }
            _ if value.is_null() => fail(path, self),
            Type::String => {
                if value.is_string() {
                    Ok(())
                } else {
                    fail(path, self)
                }
            }
            Type::Int => {
                if value.is_i64() {
                    Ok(())
                } else {
                    fail(path, self)
                }
            }
            Type::Float => {
                if value.is_number() {
                    Ok(())
                } else {
                    fail(path, self)
                }
            }
            Type::Bool => {
                if value.is_boolean() {
                    Ok(())
                } else {
                    fail(path, self)
                }
            }
            Type::Duration => {
                if value.is_u64() {
                    Ok(())
                } else {
                    fail(path, self)
                }
            }
            Type::Bytes => match value.as_str() {
                Some(s) if is_base64(s) => Ok(()),
                _ => fail(path, self),
            },
            Type::Timestamp => match value.as_str() {
                Some(s) if is_timestamp(s) => Ok(()),
                _ => fail(path, self),
            },
            Type::Uuid => match value.as_str() {
                Some(s) if is_uuid(s) => Ok(()),
                _ => fail(path, self),
            },
            Type::List { item } => {
                let Some(items) = value.as_array() else {
                    return fail(path, self);
                };
                for (i, v) in items.iter().enumerate() {
                    let len = path.len();
                    path.push_str(&format!("[{i}]"));
                    item.validate_at(v, strict, path)?;
                    path.truncate(len);
                }
                Ok(())
            }
            Type::Map { value: vt, .. } => {
                let Some(obj) = value.as_object() else {
                    return fail(path, self);
                };
                for (k, v) in obj {
                    let len = path.len();
                    path.push_str(&format!("[{k:?}]"));
                    vt.validate_at(v, strict, path)?;
                    path.truncate(len);
                }
                Ok(())
            }
            Type::Record { fields, .. } => {
                let Some(obj) = value.as_object() else {
                    return fail(path, self);
                };
                for f in fields {
                    let len = path.len();
                    path.push('.');
                    path.push_str(&f.name);
                    match obj.get(&f.name) {
                        Some(v) => f.ty.validate_at(v, strict, path)?,
                        None if f.required() => {
                            return Err(ValidationError {
                                path: path.clone(),
                                message: format!(
                                    "missing required field `{}` of type {}",
                                    f.name, f.ty
                                ),
                            })
                        }
                        None => {}
                    }
                    path.truncate(len);
                }
                if strict {
                    if let Some(k) = obj.keys().find(|k| !fields.iter().any(|f| &f.name == *k)) {
                        return Err(ValidationError {
                            path: path.clone(),
                            message: format!("unknown field `{k}`"),
                        });
                    }
                }
                Ok(())
            }
        }
    }

    /// Fill in declared defaults (recursively) for record fields that are absent.
    pub fn apply_defaults(&self, value: &mut Value) {
        match (self, value) {
            (Type::Optional { inner }, v) if !v.is_null() => inner.apply_defaults(v),
            (Type::List { item }, Value::Array(items)) => {
                items.iter_mut().for_each(|v| item.apply_defaults(v))
            }
            (Type::Map { value: vt, .. }, Value::Object(obj)) => {
                obj.values_mut().for_each(|v| vt.apply_defaults(v))
            }
            (Type::Record { fields, .. }, Value::Object(obj)) => {
                for f in fields {
                    match obj.get_mut(&f.name) {
                        Some(v) => f.ty.apply_defaults(v),
                        None => {
                            if let Some(d) = &f.default {
                                obj.insert(f.name.clone(), d.clone());
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

impl fmt::Display for Type {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Type::String => write!(f, "String"),
            Type::Int => write!(f, "Int"),
            Type::Float => write!(f, "Float"),
            Type::Bool => write!(f, "Bool"),
            Type::Bytes => write!(f, "Bytes"),
            Type::Duration => write!(f, "Duration"),
            Type::Timestamp => write!(f, "Timestamp"),
            Type::Uuid => write!(f, "UUID"),
            Type::Json => write!(f, "JSON"),
            Type::Null => write!(f, "null"),
            Type::List { item } => write!(f, "List<{item}>"),
            Type::Map { key, value } => write!(f, "Map<{key}, {value}>"),
            Type::Optional { inner } => write!(f, "{inner}?"),
            Type::Record { name: Some(n), .. } => write!(f, "{n}"),
            Type::Record { name: None, fields } => {
                write!(f, "{{ ")?;
                for (i, fl) in fields.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}: {}", fl.name, fl.ty)?;
                }
                write!(f, " }}")
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ValidationError {
    pub path: String,
    pub message: String,
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path, self.message)
    }
}

impl std::error::Error for ValidationError {}

pub fn describe(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(n) if n.is_i64() || n.is_u64() => "an integer",
        Value::Number(_) => "a float",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

pub fn is_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, c)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                *c == b'-'
            } else {
                c.is_ascii_hexdigit()
            }
        })
}

/// RFC 3339 `YYYY-MM-DDTHH:MM:SS[.frac](Z|±HH:MM)`.
pub fn is_timestamp(s: &str) -> bool {
    let b = s.as_bytes();
    let digits =
        |r: std::ops::Range<usize>| r.clone().all(|i| b.get(i).is_some_and(u8::is_ascii_digit));
    if b.len() < 20
        || !digits(0..4)
        || b[4] != b'-'
        || !digits(5..7)
        || b[7] != b'-'
        || !digits(8..10)
    {
        return false;
    }
    if !matches!(b[10], b'T' | b't' | b' ')
        || !digits(11..13)
        || b[13] != b':'
        || !digits(14..16)
        || b[16] != b':'
        || !digits(17..19)
    {
        return false;
    }
    let mut i = 19;
    if b.get(i) == Some(&b'.') {
        i += 1;
        let start = i;
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        if i == start {
            return false;
        }
    }
    match b.get(i) {
        Some(b'Z' | b'z') => i + 1 == b.len(),
        Some(b'+' | b'-') => {
            b.len() == i + 6 && digits(i + 1..i + 3) && b[i + 3] == b':' && digits(i + 4..i + 6)
        }
        _ => false,
    }
}

pub fn is_base64(s: &str) -> bool {
    s.len().is_multiple_of(4)
        && s.trim_end_matches('=')
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'+' || c == b'/')
        && s.len() - s.trim_end_matches('=').len() <= 2
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn order() -> Type {
        Type::record(
            Some("Order"),
            vec![
                FieldType {
                    name: "id".into(),
                    ty: Type::Uuid,
                    default: None,
                },
                FieldType {
                    name: "qty".into(),
                    ty: Type::Int,
                    default: Some(json!(1)),
                },
                FieldType {
                    name: "note".into(),
                    ty: Type::String.optional(),
                    default: None,
                },
                FieldType {
                    name: "tags".into(),
                    ty: Type::List {
                        item: Box::new(Type::String),
                    },
                    default: None,
                },
            ],
        )
    }

    #[test]
    fn validates_records() {
        let t = order();
        let id = "123e4567-e89b-12d3-a456-426614174000";
        assert!(t.validate(&json!({"id": id, "tags": []}), true).is_ok());
        let e = t
            .validate(&json!({"id": id, "tags": [1]}), true)
            .unwrap_err();
        assert_eq!(e.path, "$.tags[0]");
        let e = t.validate(&json!({"tags": []}), true).unwrap_err();
        assert!(e.message.contains("missing required field `id`"));
        assert!(t
            .validate(&json!({"id": id, "tags": [], "x": 1}), true)
            .is_err());
        assert!(t
            .validate(&json!({"id": id, "tags": [], "x": 1}), false)
            .is_ok());
        assert!(t
            .validate(&json!({"id": "nope", "tags": []}), true)
            .is_err());
    }

    #[test]
    fn defaults() {
        let mut v = json!({"id": "x", "tags": []});
        order().apply_defaults(&mut v);
        assert_eq!(v["qty"], json!(1));
    }

    #[test]
    fn formats() {
        assert!(is_timestamp("2026-11-14T10:00:00Z"));
        assert!(is_timestamp("2026-11-14T10:00:00.123+05:30"));
        assert!(!is_timestamp("2026-11-14"));
        assert!(is_base64("aGVsbG8="));
        assert!(!is_base64("abc"));
    }
}
