//! A JSON Schema validator for the subset of draft 2020-12 the harness's
//! schemas use.
//!
//! Supported keywords: `type` (a name or a list), `enum`, `const`,
//! `properties`, `required`, `additionalProperties` (`false` or a
//! schema), `items`, `minItems`, `maxItems`, `minLength`, `minimum`,
//! `maximum`, `exclusiveMinimum` and `$ref` to `#/$defs/<name>`. The
//! annotations `$schema`, `$id`, `$defs`, `$comment`, `title`,
//! `description` and `default` are allowed and ignored.
//!
//! A schema that uses any other keyword is reported as a problem rather
//! than half-checked: a schema file cannot quietly ask for a check this
//! validator does not make. CI also checks the published results with an
//! independent, complete validator (`check-jsonschema`).

use serde_json::{Map, Value};

const ANNOTATIONS: &[&str] = &[
    "$schema",
    "$id",
    "$defs",
    "$comment",
    "title",
    "description",
    "default",
];

const KEYWORDS: &[&str] = &[
    "type",
    "enum",
    "const",
    "properties",
    "required",
    "additionalProperties",
    "items",
    "minItems",
    "maxItems",
    "minLength",
    "minimum",
    "maximum",
    "exclusiveMinimum",
    "$ref",
];

/// Every way `value` breaks `schema`, each as `<JSON pointer>: <why>`.
/// Empty when it conforms.
pub fn validate(schema: &Value, value: &Value) -> Vec<String> {
    let mut problems = Vec::new();
    check(schema, schema, value, "", &mut problems, 0);
    problems
}

fn check(
    root: &Value,
    schema: &Value,
    value: &Value,
    at: &str,
    out: &mut Vec<String>,
    depth: usize,
) {
    let here = if at.is_empty() { "/" } else { at };
    if depth > 64 {
        out.push(format!("{here}: the schema nests too deeply"));
        return;
    }
    let schema = match schema {
        Value::Bool(true) => return,
        Value::Bool(false) => {
            out.push(format!("{here}: not allowed"));
            return;
        }
        Value::Object(schema) => schema,
        _ => {
            out.push(format!("{here}: the schema is not an object"));
            return;
        }
    };
    for key in schema.keys() {
        if !KEYWORDS.contains(&key.as_str()) && !ANNOTATIONS.contains(&key.as_str()) {
            out.push(format!(
                "{here}: the schema uses {key:?}, which this validator does not check"
            ));
        }
    }
    if let Some(reference) = schema.get("$ref") {
        let target = reference
            .as_str()
            .and_then(|r| r.strip_prefix("#/$defs/"))
            .and_then(|name| root.get("$defs")?.get(name));
        match target {
            Some(target) => check(root, target, value, at, out, depth + 1),
            None => out.push(format!("{here}: cannot resolve $ref {reference}")),
        }
    }
    if let Some(types) = schema.get("type") {
        let names: Vec<&str> = match types {
            Value::String(name) => vec![name.as_str()],
            Value::Array(names) => names.iter().filter_map(Value::as_str).collect(),
            _ => vec![],
        };
        if !names.iter().any(|name| is_type(value, name)) {
            out.push(format!(
                "{here}: expected {}, found {}",
                names.join(" or "),
                type_of(value)
            ));
            return;
        }
    }
    if let Some(Value::Array(allowed)) = schema.get("enum")
        && !allowed.contains(value)
    {
        out.push(format!(
            "{here}: {value} is not one of {}",
            Value::Array(allowed.clone())
        ));
    }
    if let Some(expected) = schema.get("const")
        && expected != value
    {
        out.push(format!("{here}: expected {expected}, found {value}"));
    }
    if let Some(n) = value.as_f64() {
        if let Some(min) = schema.get("minimum").and_then(Value::as_f64)
            && n < min
        {
            out.push(format!("{here}: {value} is below the minimum {min}"));
        }
        if let Some(max) = schema.get("maximum").and_then(Value::as_f64)
            && n > max
        {
            out.push(format!("{here}: {value} is above the maximum {max}"));
        }
        if let Some(min) = schema.get("exclusiveMinimum").and_then(Value::as_f64)
            && n <= min
        {
            out.push(format!("{here}: {value} is not above {min}"));
        }
    }
    if let (Value::String(s), Some(min)) = (value, schema.get("minLength").and_then(Value::as_u64))
        && (s.chars().count() as u64) < min
    {
        out.push(format!("{here}: shorter than {min} characters"));
    }
    if let Value::Array(items) = value {
        if let Some(min) = schema.get("minItems").and_then(Value::as_u64)
            && (items.len() as u64) < min
        {
            out.push(format!("{here}: fewer than {min} items"));
        }
        if let Some(max) = schema.get("maxItems").and_then(Value::as_u64)
            && (items.len() as u64) > max
        {
            out.push(format!("{here}: more than {max} items"));
        }
        if let Some(item) = schema.get("items") {
            for (i, element) in items.iter().enumerate() {
                check(root, item, element, &format!("{at}/{i}"), out, depth + 1);
            }
        }
    }
    if let Value::Object(fields) = value {
        check_object(root, schema, fields, at, out, depth);
    }
}

fn check_object(
    root: &Value,
    schema: &Map<String, Value>,
    fields: &Map<String, Value>,
    at: &str,
    out: &mut Vec<String>,
    depth: usize,
) {
    let here = if at.is_empty() { "/" } else { at };
    let properties = schema.get("properties").and_then(Value::as_object);
    if let Some(Value::Array(required)) = schema.get("required") {
        for name in required.iter().filter_map(Value::as_str) {
            if !fields.contains_key(name) {
                out.push(format!("{here}: missing {name:?}"));
            }
        }
    }
    for (name, field) in fields {
        let path = format!("{at}/{}", name.replace('~', "~0").replace('/', "~1"));
        match properties.and_then(|p| p.get(name)) {
            Some(property) => check(root, property, field, &path, out, depth + 1),
            None => {
                if let Some(extra) = schema.get("additionalProperties") {
                    match extra {
                        Value::Bool(false) => out.push(format!("{path}: not allowed")),
                        other => check(root, other, field, &path, out, depth + 1),
                    }
                }
            }
        }
    }
}

fn is_type(value: &Value, name: &str) -> bool {
    match name {
        "null" => value.is_null(),
        "boolean" => value.is_boolean(),
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "number" => value.is_number(),
        "integer" => {
            value.is_u64() || value.is_i64() || value.as_f64().is_some_and(|f| f.fract() == 0.0)
        }
        _ => false,
    }
}

fn type_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) if n.is_f64() => "number",
        Value::Number(_) => "integer",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn problems(schema: Value, value: Value) -> Vec<String> {
        validate(&schema, &value)
    }

    #[test]
    fn checks_types_and_values() {
        let schema = json!({
            "type": "object",
            "required": ["a", "n"],
            "additionalProperties": false,
            "properties": {
                "a": {"type": "string", "minLength": 2},
                "n": {"type": "integer", "minimum": 0, "maximum": 10},
                "f": {"type": ["number", "null"], "exclusiveMinimum": 0},
                "e": {"enum": ["x", "y"]},
                "c": {"const": 3},
                "l": {"type": "array", "items": {"type": "boolean"}, "minItems": 1, "maxItems": 2}
            }
        });
        assert!(
            problems(
                schema.clone(),
                json!({"a": "ab", "n": 3, "f": 1.5, "e": "x", "c": 3, "l": [true]})
            )
            .is_empty()
        );
        assert!(problems(schema.clone(), json!({"a": "ab", "n": 3.0, "f": null})).is_empty());
        let got = problems(
            schema.clone(),
            json!({"a": "a", "n": 11, "f": 0, "e": "z", "c": 4, "l": [], "zz": 1}),
        );
        let joined = got.join("\n");
        for needle in [
            "/a: shorter than 2",
            "/n: 11 is above",
            "/f: 0 is not above 0",
            "/e: \"z\" is not one of",
            "/c: expected 3",
            "/l: fewer than 1",
            "/zz: not allowed",
        ] {
            assert!(joined.contains(needle), "{needle} in {joined}");
        }
        let got = problems(schema.clone(), json!({"n": -1, "l": [true, false, 1]})).join("\n");
        for needle in [
            "/: missing \"a\"",
            "/n: -1 is below",
            "/l: more than 2",
            "/l/2: expected boolean, found integer",
        ] {
            assert!(got.contains(needle), "{needle} in {got}");
        }
        assert_eq!(
            problems(schema, json!([])),
            ["/: expected object, found array"]
        );
        assert_eq!(
            problems(json!({"type": "integer"}), json!(1.5)),
            ["/: expected integer, found number"]
        );
        assert_eq!(
            problems(json!({"type": "string"}), json!(null)),
            ["/: expected string, found null"]
        );
        assert!(problems(json!({"type": "boolean"}), json!({}))[0].contains("found object"));
    }

    #[test]
    fn follows_refs_and_maps() {
        let schema = json!({
            "$defs": {"pos": {"type": "number", "minimum": 0}},
            "type": "object",
            "additionalProperties": {"$ref": "#/$defs/pos"}
        });
        assert!(problems(schema.clone(), json!({"a": 1, "b/c": 2})).is_empty());
        assert_eq!(
            problems(schema.clone(), json!({"b/c": -1})),
            ["/b~1c: -1 is below the minimum 0"]
        );
        assert!(problems(json!({"$ref": "#/$defs/none"}), json!(1))[0].contains("cannot resolve"));
        assert!(problems(json!(true), json!(1)).is_empty());
        assert_eq!(problems(json!(false), json!(1)), ["/: not allowed"]);
        assert!(problems(json!(3), json!(1))[0].contains("not an object"));
    }

    #[test]
    fn refuses_keywords_it_does_not_check() {
        let got = problems(json!({"pattern": "^a$", "title": "t"}), json!("b"));
        assert_eq!(got.len(), 1);
        assert!(got[0].contains("\"pattern\""), "{got:?}");
    }

    #[test]
    fn stops_on_a_ref_loop() {
        let schema = json!({"$defs": {"x": {"$ref": "#/$defs/x"}}, "$ref": "#/$defs/x"});
        assert!(
            problems(schema, json!(1))
                .iter()
                .any(|p| p.contains("too deeply"))
        );
    }
}
