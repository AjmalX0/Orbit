//! Resolution of syntactic types into IR types, and type compatibility rules.

use orbit_ir::{FieldType, Type};
use orbit_syntax::ast::{self, TypeExpr};
use orbit_syntax::diagnostic::{suggest, Diagnostic};
use std::collections::{BTreeMap, HashSet};

pub const PRIMITIVES: &[&str] = &[
    "String",
    "Int",
    "Float",
    "Bool",
    "Bytes",
    "Duration",
    "Timestamp",
    "UUID",
    "JSON",
];

pub fn primitive(name: &str) -> Option<Type> {
    Some(match name {
        "String" => Type::String,
        "Int" => Type::Int,
        "Float" => Type::Float,
        "Bool" => Type::Bool,
        "Bytes" => Type::Bytes,
        "Duration" => Type::Duration,
        "Timestamp" => Type::Timestamp,
        "UUID" => Type::Uuid,
        "JSON" => Type::Json,
        _ => return None,
    })
}

/// All `schema` declarations of a compilation.
pub struct Schemas<'a> {
    pub decls: BTreeMap<String, &'a ast::Schema>,
}

impl<'a> Schemas<'a> {
    pub fn resolve(&self, ty: &TypeExpr, diags: &mut Vec<Diagnostic>) -> Type {
        self.resolve_inner(ty, diags, &mut vec![])
    }

    fn resolve_inner(
        &self,
        ty: &TypeExpr,
        diags: &mut Vec<Diagnostic>,
        stack: &mut Vec<String>,
    ) -> Type {
        match ty {
            TypeExpr::Named(id) => {
                if let Some(p) = primitive(&id.name) {
                    return p;
                }
                if let Some(schema) = self.decls.get(&id.name) {
                    if stack.contains(&id.name) {
                        let mut cycle = stack.clone();
                        cycle.push(id.name.clone());
                        diags.push(
                            Diagnostic::error("E0202", format!("recursive schema `{}`", id.name), id.span)
                                .with_note(format!("cycle: {}", cycle.join(" -> ")))
                                .with_help("schemas must be finite; break the cycle with a `JSON` or `String` id field"),
                        );
                        return Type::Json;
                    }
                    stack.push(id.name.clone());
                    let fields = self.fields(&schema.fields, diags, stack);
                    stack.pop();
                    return Type::record(Some(&id.name), fields);
                }
                let mut d =
                    Diagnostic::error("E0201", format!("unknown type `{}`", id.name), id.span);
                let candidates: Vec<&str> = PRIMITIVES
                    .iter()
                    .copied()
                    .chain(self.decls.keys().map(String::as_str))
                    .chain(["List", "Map"])
                    .collect();
                if let Some(s) = suggest(&id.name, candidates.iter().copied()) {
                    d = d.with_help(format!("did you mean `{s}`?"));
                } else if id.name == "List" || id.name == "Map" {
                    d = d.with_help(format!(
                        "`{}` needs type arguments, e.g. `List<String>`",
                        id.name
                    ));
                } else {
                    d = d.with_help(format!("declare it with `schema {} {{ ... }}`", id.name));
                }
                diags.push(d);
                Type::Json
            }
            TypeExpr::List(inner, _) => Type::List {
                item: Box::new(self.resolve_inner(inner, diags, stack)),
            },
            TypeExpr::Map(k, v, span) => {
                let key = self.resolve_inner(k, diags, stack);
                if key != Type::String {
                    diags.push(
                        Diagnostic::error(
                            "E0204",
                            format!("map keys must be `String`, found `{key}`"),
                            *span,
                        )
                        .with_note("maps are encoded as JSON objects, whose keys are strings"),
                    );
                }
                Type::Map {
                    key: Box::new(Type::String),
                    value: Box::new(self.resolve_inner(v, diags, stack)),
                }
            }
            TypeExpr::Optional(inner, _) => self.resolve_inner(inner, diags, stack).optional(),
            TypeExpr::Record(fields, _) => Type::record(None, self.fields(fields, diags, stack)),
        }
    }

    pub fn fields(
        &self,
        fields: &[ast::Field],
        diags: &mut Vec<Diagnostic>,
        stack: &mut Vec<String>,
    ) -> Vec<FieldType> {
        let mut seen: Vec<&ast::Field> = vec![];
        let mut out = vec![];
        for f in fields {
            if let Some(prev) = seen.iter().find(|p| p.name.name == f.name.name) {
                diags.push(
                    Diagnostic::error(
                        "E0203",
                        format!("duplicate field `{}`", f.name.name),
                        f.name.span,
                    )
                    .with_label(prev.name.span, "first declared here"),
                );
                continue;
            }
            seen.push(f);
            let ty = self.resolve_inner(&f.ty, diags, stack);
            let default = f
                .default
                .as_ref()
                .and_then(|d| crate::check::constant(d, &ty, diags));
            out.push(FieldType {
                name: f.name.name.clone(),
                ty,
                default,
            });
        }
        out
    }
}

/// Can a value of type `src` be used where `dst` is expected?
/// Returns a human readable reason on failure.
pub fn assignable(src: &Type, dst: &Type) -> Result<(), String> {
    match (src, dst) {
        (_, Type::Json) | (Type::Json, _) => Ok(()),
        (Type::Null, Type::Optional { .. }) => Ok(()),
        (Type::Null, _) => Err(format!(
            "`null` is not a valid `{dst}`; make the target optional (`{dst}?`)"
        )),
        (Type::Optional { inner: a }, Type::Optional { inner: b }) => assignable(a, b),
        (_, Type::Optional { inner }) => assignable(src, inner),
        (Type::Optional { inner }, _) => match assignable(inner, dst) {
            Ok(()) => Err(format!("`{src}` may be null but `{dst}` is required")),
            Err(e) => Err(e),
        },
        (Type::Int, Type::Float) => Ok(()),
        (Type::List { item: a }, Type::List { item: b }) => {
            assignable(a, b).map_err(|e| format!("list item: {e}"))
        }
        (Type::Map { value: a, .. }, Type::Map { value: b, .. }) => {
            assignable(a, b).map_err(|e| format!("map value: {e}"))
        }
        (Type::Record { fields: sf, .. }, Type::Record { fields: df, .. }) => {
            for d in df {
                match sf.iter().find(|s| s.name == d.name) {
                    Some(s) => {
                        assignable(&s.ty, &d.ty).map_err(|e| format!("field `{}`: {e}", d.name))?
                    }
                    None if d.required() => {
                        return Err(format!("missing required field `{}: {}`", d.name, d.ty))
                    }
                    None => {}
                }
            }
            Ok(())
        }
        (a, b) if a == b => Ok(()),
        _ => Err(format!("expected `{dst}`, found `{src}`")),
    }
}

/// Least upper bound used for `if` branches, list items and `coalesce`.
pub fn join(a: &Type, b: &Type) -> Option<Type> {
    match (a, b) {
        _ if a == b => Some(a.clone()),
        (Type::Json, _) | (_, Type::Json) => Some(Type::Json),
        (Type::Null, t) | (t, Type::Null) => Some(t.clone().optional()),
        (Type::Optional { inner }, t) | (t, Type::Optional { inner }) => {
            join(inner, t).map(Type::optional)
        }
        (Type::Int, Type::Float) | (Type::Float, Type::Int) => Some(Type::Float),
        (Type::List { item: x }, Type::List { item: y }) => {
            join(x, y).map(|t| Type::List { item: Box::new(t) })
        }
        (Type::Record { .. }, Type::Record { .. }) => {
            if assignable(a, b).is_ok() {
                Some(b.clone())
            } else if assignable(b, a).is_ok() {
                Some(a.clone())
            } else {
                None
            }
        }
        _ => None,
    }
}

pub fn field_names(t: &Type) -> Vec<&str> {
    match t.unwrap_optional() {
        Type::Record { fields, .. } => fields.iter().map(|f| f.name.as_str()).collect(),
        _ => vec![],
    }
}

/// Languages ORBIT knows SDKs or adapters for (used only for warnings).
pub const KNOWN_LANGUAGES: &[&str] = &[
    "python",
    "typescript",
    "javascript",
    "go",
    "rust",
    "java",
    "kotlin",
    "csharp",
    "ruby",
    "php",
    "elixir",
    "swift",
    "cpp",
    "http",
    "any",
];

pub fn unique<'a>(names: impl IntoIterator<Item = &'a str>) -> HashSet<&'a str> {
    names.into_iter().collect()
}
