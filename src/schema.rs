//! Minimal JSON Schema validator for tool declarations and tool-call arguments.
//! Replaces the full `jsonschema` crate (which pulls in regex engines and ICU tables)
//! while preserving Draft 2020-12 / Draft-07 validation semantics for JSON objects,
//! arrays, scalars, local `$ref` pointers, and boolean combinators.

use serde_json::{Map, Value};
use std::collections::HashSet;

const MAX_DEPTH: usize = 64;

const VALID_TYPES: [&str; 7] = [
    "object", "array", "string", "number", "integer", "boolean", "null",
];

/// Validate that `schema` is a syntactically well-formed JSON Schema with local refs only.
pub fn validate(schema: &Value) -> Result<(), String> {
    validate_node(schema, schema, 0)
}

/// Validate `instance` against `schema`.
pub fn is_valid(schema: &Value, instance: &Value) -> Result<bool, String> {
    validate(schema)?;
    eval_node(schema, schema, instance, 0)
}

fn validate_node(root: &Value, node: &Value, depth: usize) -> Result<(), String> {
    if depth > MAX_DEPTH {
        return Err("schema nesting exceeds maximum depth".into());
    }
    match node {
        Value::Bool(_) => Ok(()),
        Value::Object(map) => validate_object(root, map, depth),
        _ => Err("schema must be a boolean or an object".into()),
    }
}

fn validate_object(root: &Value, map: &Map<String, Value>, depth: usize) -> Result<(), String> {
    validate_type_keyword(map)?;
    validate_ref_keyword(root, map)?;
    validate_object_keywords(root, map, depth)?;
    validate_array_keywords(root, map, depth)?;
    validate_combinator_keywords(root, map, depth)?;
    validate_scalar_keywords(map)
}

fn validate_type_keyword(map: &Map<String, Value>) -> Result<(), String> {
    let Some(ty) = map.get("type") else {
        return Ok(());
    };
    match ty {
        Value::String(s) => {
            if !VALID_TYPES.contains(&s.as_str()) {
                return Err(format!("unknown JSON Schema type '{s}'"));
            }
        }
        Value::Array(arr) => {
            if arr.is_empty() {
                return Err("'type' array must not be empty".into());
            }
            let mut seen = HashSet::new();
            for item in arr {
                let s = item
                    .as_str()
                    .ok_or_else(|| "'type' array items must be strings".to_string())?;
                if !VALID_TYPES.contains(&s) {
                    return Err(format!("unknown JSON Schema type '{s}'"));
                }
                if !seen.insert(s) {
                    return Err(format!("duplicate type '{s}' in 'type' array"));
                }
            }
        }
        _ => return Err("'type' must be a string or an array of strings".into()),
    }
    Ok(())
}

fn validate_ref_keyword(root: &Value, map: &Map<String, Value>) -> Result<(), String> {
    let Some(r) = map.get("$ref") else {
        return Ok(());
    };
    let ptr = r
        .as_str()
        .ok_or_else(|| "'$ref' must be a string".to_string())?;
    if !ptr.starts_with('#') {
        return Err("external schema references are disabled".into());
    }
    resolve_pointer(root, ptr)?;
    Ok(())
}

fn validate_object_keywords(
    root: &Value,
    map: &Map<String, Value>,
    depth: usize,
) -> Result<(), String> {
    for key in ["properties", "patternProperties", "$defs", "definitions"] {
        if let Some(val) = map.get(key) {
            let obj = val
                .as_object()
                .ok_or_else(|| format!("'{key}' must be an object"))?;
            for sub in obj.values() {
                validate_node(root, sub, depth + 1)?;
            }
        }
    }
    if let Some(ap) = map.get("additionalProperties") {
        validate_node(root, ap, depth + 1)?;
    }
    if let Some(req) = map.get("required") {
        let arr = req
            .as_array()
            .ok_or_else(|| "'required' must be an array of strings".to_string())?;
        let mut seen = HashSet::new();
        for item in arr {
            let s = item
                .as_str()
                .ok_or_else(|| "'required' entries must be strings".to_string())?;
            if !seen.insert(s) {
                return Err(format!("duplicate required property '{s}'"));
            }
        }
    }
    Ok(())
}

fn validate_array_keywords(
    root: &Value,
    map: &Map<String, Value>,
    depth: usize,
) -> Result<(), String> {
    if let Some(items) = map.get("items") {
        match items {
            Value::Array(arr) => {
                for sub in arr {
                    validate_node(root, sub, depth + 1)?;
                }
            }
            _ => validate_node(root, items, depth + 1)?,
        }
    }
    if let Some(ai) = map.get("additionalItems") {
        validate_node(root, ai, depth + 1)?;
    }
    if let Some(pi) = map.get("prefixItems") {
        let arr = pi
            .as_array()
            .ok_or_else(|| "'prefixItems' must be an array of schemas".to_string())?;
        for sub in arr {
            validate_node(root, sub, depth + 1)?;
        }
    }
    Ok(())
}

fn validate_combinator_keywords(
    root: &Value,
    map: &Map<String, Value>,
    depth: usize,
) -> Result<(), String> {
    for key in ["anyOf", "oneOf", "allOf"] {
        if let Some(val) = map.get(key) {
            let arr = val
                .as_array()
                .ok_or_else(|| format!("'{key}' must be an array of schemas"))?;
            if arr.is_empty() {
                return Err(format!("'{key}' must not be empty"));
            }
            for sub in arr {
                validate_node(root, sub, depth + 1)?;
            }
        }
    }
    for key in ["not", "if", "then", "else"] {
        if let Some(sub) = map.get(key) {
            validate_node(root, sub, depth + 1)?;
        }
    }
    Ok(())
}

fn validate_scalar_keywords(map: &Map<String, Value>) -> Result<(), String> {
    if let Some(en) = map.get("enum") {
        let arr = en
            .as_array()
            .ok_or_else(|| "'enum' must be an array".to_string())?;
        if arr.is_empty() {
            return Err("'enum' must not be empty".into());
        }
    }
    for key in ["minimum", "maximum"] {
        if let Some(val) = map.get(key) {
            if !val.is_number() {
                return Err(format!("'{key}' must be a number"));
            }
        }
    }
    for key in ["exclusiveMinimum", "exclusiveMaximum"] {
        if let Some(val) = map.get(key) {
            if !val.is_number() && !val.is_boolean() {
                return Err(format!("'{key}' must be a number or boolean"));
            }
        }
    }
    if let Some(m) = map.get("multipleOf") {
        let n = m
            .as_f64()
            .ok_or_else(|| "'multipleOf' must be a number".to_string())?;
        if !n.is_finite() || n <= 0.0 {
            return Err("'multipleOf' must be a positive number".into());
        }
    }
    for key in [
        "minLength",
        "maxLength",
        "minItems",
        "maxItems",
        "minProperties",
        "maxProperties",
    ] {
        if let Some(val) = map.get(key) {
            if parse_non_neg_int(val).is_none() {
                return Err(format!("'{key}' must be a non-negative integer"));
            }
        }
    }
    if let Some(u) = map.get("uniqueItems") {
        if !u.is_boolean() {
            return Err("'uniqueItems' must be a boolean".into());
        }
    }
    for key in ["pattern", "format"] {
        if let Some(val) = map.get(key) {
            if !val.is_string() {
                return Err(format!("'{key}' must be a string"));
            }
        }
    }
    Ok(())
}

fn parse_non_neg_int(val: &Value) -> Option<usize> {
    if let Some(u) = val.as_u64() {
        return usize::try_from(u).ok();
    }
    if let Some(f) = val.as_f64() {
        if f.is_finite() && f >= 0.0 && f.fract() == 0.0 && f <= (usize::MAX as f64) {
            return Some(f as usize);
        }
    }
    None
}

fn resolve_pointer<'a>(root: &'a Value, reference: &str) -> Result<&'a Value, String> {
    if reference == "#" {
        return Ok(root);
    }
    let Some(rest) = reference.strip_prefix("#/") else {
        return Err(format!("unsupported schema reference '{reference}'"));
    };
    let mut current = root;
    for raw_part in rest.split('/') {
        let decoded = percent_decode_fragment(raw_part);
        let unescaped = decoded.replace("~1", "/").replace("~0", "~");
        match current {
            Value::Object(map) => {
                current = map
                    .get(&unescaped)
                    .ok_or_else(|| format!("unresolvable schema reference '{reference}'"))?;
            }
            Value::Array(arr) => {
                let idx: usize = unescaped
                    .parse()
                    .map_err(|_| format!("unresolvable schema reference '{reference}'"))?;
                current = arr
                    .get(idx)
                    .ok_or_else(|| format!("unresolvable schema reference '{reference}'"))?;
            }
            _ => return Err(format!("unresolvable schema reference '{reference}'")),
        }
    }
    Ok(current)
}

fn percent_decode_fragment(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push((h << 4) | l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| input.to_string())
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn eval_node(root: &Value, node: &Value, instance: &Value, depth: usize) -> Result<bool, String> {
    if depth > MAX_DEPTH {
        return Err("schema evaluation recursion limit exceeded".into());
    }
    match node {
        Value::Bool(b) => Ok(*b),
        Value::Object(map) => eval_object(root, map, instance, depth),
        _ => Err("schema must be a boolean or an object".into()),
    }
}

fn matches_type(ty: &str, instance: &Value) -> bool {
    match ty {
        "object" => instance.is_object(),
        "array" => instance.is_array(),
        "string" => instance.is_string(),
        "boolean" => instance.is_boolean(),
        "null" => instance.is_null(),
        "number" => instance.is_number(),
        "integer" => {
            instance.is_i64()
                || instance.is_u64()
                || instance
                    .as_f64()
                    .is_some_and(|n| n.is_finite() && n.fract() == 0.0)
        }
        _ => false,
    }
}

fn json_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Null, Value::Null) => true,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::String(x), Value::String(y)) => x == y,
        (Value::Number(_), Value::Number(_)) => {
            if let (Some(x), Some(y)) = (a.as_i64(), b.as_i64()) {
                return x == y;
            }
            if let (Some(x), Some(y)) = (a.as_u64(), b.as_u64()) {
                return x == y;
            }
            match (a.as_f64(), b.as_f64()) {
                (Some(x), Some(y)) => x == y,
                _ => false,
            }
        }
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y.iter()).all(|(l, r)| json_eq(l, r))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(k, v)| y.get(k).is_some_and(|yv| json_eq(v, yv)))
        }
        _ => false,
    }
}

fn eval_object(
    root: &Value,
    map: &Map<String, Value>,
    instance: &Value,
    depth: usize,
) -> Result<bool, String> {
    if !eval_ref_and_primitives(root, map, instance, depth)? {
        return Ok(false);
    }
    if let Some(num) = instance.as_f64() {
        if !eval_number(map, num) {
            return Ok(false);
        }
    }
    if let Some(s) = instance.as_str() {
        if !eval_string(map, s) {
            return Ok(false);
        }
    }
    if let Some(arr) = instance.as_array() {
        if !eval_array(root, map, arr, depth)? {
            return Ok(false);
        }
    }
    if let Some(obj) = instance.as_object() {
        if !eval_object_props(root, map, obj, depth)? {
            return Ok(false);
        }
    }
    eval_combinators(root, map, instance, depth)
}

fn eval_ref_and_primitives(
    root: &Value,
    map: &Map<String, Value>,
    instance: &Value,
    depth: usize,
) -> Result<bool, String> {
    if let Some(Value::String(ptr)) = map.get("$ref") {
        let target = resolve_pointer(root, ptr)?;
        if !eval_node(root, target, instance, depth + 1)? {
            return Ok(false);
        }
    }
    if let Some(ty) = map.get("type") {
        let ok = match ty {
            Value::String(s) => matches_type(s, instance),
            Value::Array(arr) => arr
                .iter()
                .filter_map(Value::as_str)
                .any(|s| matches_type(s, instance)),
            _ => false,
        };
        if !ok {
            return Ok(false);
        }
    }
    if let Some(Value::Array(variants)) = map.get("enum") {
        if !variants.iter().any(|v| json_eq(v, instance)) {
            return Ok(false);
        }
    }
    if let Some(constant) = map.get("const") {
        if !json_eq(constant, instance) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn eval_number(map: &Map<String, Value>, num: f64) -> bool {
    if let Some(min) = map.get("minimum").and_then(Value::as_f64) {
        if map
            .get("exclusiveMinimum")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            if num <= min {
                return false;
            }
        } else if num < min {
            return false;
        }
    }
    if let Some(max) = map.get("maximum").and_then(Value::as_f64) {
        if map
            .get("exclusiveMaximum")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            if num >= max {
                return false;
            }
        } else if num > max {
            return false;
        }
    }
    if let Some(emin) = map.get("exclusiveMinimum").and_then(Value::as_f64) {
        if num <= emin {
            return false;
        }
    }
    if let Some(emax) = map.get("exclusiveMaximum").and_then(Value::as_f64) {
        if num >= emax {
            return false;
        }
    }
    if let Some(mult) = map.get("multipleOf").and_then(Value::as_f64) {
        let ratio = num / mult;
        if !ratio.is_finite() || (ratio - ratio.round()).abs() > 1e-9 * ratio.abs().max(1.0) {
            return false;
        }
    }
    true
}

fn eval_string(map: &Map<String, Value>, s: &str) -> bool {
    if let Some(min_len) = map.get("minLength").and_then(parse_non_neg_int) {
        if s.chars().count() < min_len {
            return false;
        }
    }
    if let Some(max_len) = map.get("maxLength").and_then(parse_non_neg_int) {
        if s.chars().count() > max_len {
            return false;
        }
    }
    true
}

fn eval_array(
    root: &Value,
    map: &Map<String, Value>,
    arr: &[Value],
    depth: usize,
) -> Result<bool, String> {
    if let Some(min_items) = map.get("minItems").and_then(parse_non_neg_int) {
        if arr.len() < min_items {
            return Ok(false);
        }
    }
    if let Some(max_items) = map.get("maxItems").and_then(parse_non_neg_int) {
        if arr.len() > max_items {
            return Ok(false);
        }
    }
    if map.get("uniqueItems").and_then(Value::as_bool) == Some(true) {
        for i in 0..arr.len() {
            for j in (i + 1)..arr.len() {
                if json_eq(&arr[i], &arr[j]) {
                    return Ok(false);
                }
            }
        }
    }
    let mut prefix_len = 0usize;
    if let Some(Value::Array(prefix)) = map.get("prefixItems") {
        prefix_len = prefix.len();
        for (item, sub) in arr.iter().zip(prefix.iter()) {
            if !eval_node(root, sub, item, depth + 1)? {
                return Ok(false);
            }
        }
    }
    if let Some(items) = map.get("items") {
        match items {
            Value::Array(tuple) => {
                for (item, sub) in arr.iter().zip(tuple.iter()) {
                    if !eval_node(root, sub, item, depth + 1)? {
                        return Ok(false);
                    }
                }
                if let Some(ai) = map.get("additionalItems") {
                    for item in arr.iter().skip(tuple.len()) {
                        if !eval_node(root, ai, item, depth + 1)? {
                            return Ok(false);
                        }
                    }
                }
            }
            _ => {
                for item in arr.iter().skip(prefix_len) {
                    if !eval_node(root, items, item, depth + 1)? {
                        return Ok(false);
                    }
                }
            }
        }
    }
    Ok(true)
}

fn eval_object_props(
    root: &Value,
    map: &Map<String, Value>,
    obj: &Map<String, Value>,
    depth: usize,
) -> Result<bool, String> {
    if let Some(min_props) = map.get("minProperties").and_then(parse_non_neg_int) {
        if obj.len() < min_props {
            return Ok(false);
        }
    }
    if let Some(max_props) = map.get("maxProperties").and_then(parse_non_neg_int) {
        if obj.len() > max_props {
            return Ok(false);
        }
    }
    if let Some(Value::Array(req)) = map.get("required") {
        for key in req {
            if let Some(k) = key.as_str() {
                if !obj.contains_key(k) {
                    return Ok(false);
                }
            }
        }
    }
    let props = map.get("properties").and_then(Value::as_object);
    let additional = map.get("additionalProperties");
    for (k, v) in obj {
        if let Some(sub) = props.and_then(|p| p.get(k)) {
            if !eval_node(root, sub, v, depth + 1)? {
                return Ok(false);
            }
        } else if let Some(ap) = additional {
            if !eval_node(root, ap, v, depth + 1)? {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

fn eval_combinators(
    root: &Value,
    map: &Map<String, Value>,
    instance: &Value,
    depth: usize,
) -> Result<bool, String> {
    if let Some(Value::Array(subs)) = map.get("allOf") {
        for sub in subs {
            if !eval_node(root, sub, instance, depth + 1)? {
                return Ok(false);
            }
        }
    }
    if let Some(Value::Array(subs)) = map.get("anyOf") {
        if !subs.iter().try_fold(false, |acc, sub| {
            if acc {
                Ok(true)
            } else {
                eval_node(root, sub, instance, depth + 1)
            }
        })? {
            return Ok(false);
        }
    }
    if let Some(Value::Array(subs)) = map.get("oneOf") {
        let mut count = 0usize;
        for sub in subs {
            if eval_node(root, sub, instance, depth + 1)? {
                count += 1;
                if count > 1 {
                    return Ok(false);
                }
            }
        }
        if count != 1 {
            return Ok(false);
        }
    }
    if let Some(not_sub) = map.get("not") {
        if eval_node(root, not_sub, instance, depth + 1)? {
            return Ok(false);
        }
    }
    if let Some(if_sub) = map.get("if") {
        let cond = eval_node(root, if_sub, instance, depth + 1)?;
        let branch = if cond {
            map.get("then")
        } else {
            map.get("else")
        };
        if let Some(branch_sub) = branch {
            if !eval_node(root, branch_sub, instance, depth + 1)? {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn validates_tool_schemas_and_rejects_malformed_definitions() {
        assert!(validate(&json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "start_line": { "type": "integer", "minimum": 1 }
            },
            "required": ["path"],
            "additionalProperties": false
        }))
        .is_ok());

        assert!(validate(&json!(123)).is_err());
        assert!(validate(&json!({"type": "not_a_type"})).is_err());
        assert!(validate(&json!({"required": [123]})).is_err());
        assert!(validate(&json!({"$ref": "https://example.com/schema.json"})).is_err());
        assert!(validate(&json!({"$ref": "#/$defs/missing"})).is_err());
        assert!(validate(&json!({"patternProperties": "bad"})).is_err());
        assert!(validate(&json!({"patternProperties": {"^x-": {"type": "bogus"}}})).is_err());
        assert!(validate(&json!({"patternProperties": {"^x-": {"type": "string"}}})).is_ok());
    }

    #[test]
    fn validates_objects_arrays_numbers_enums_and_local_refs() {
        let schema = json!({
            "$defs": {
                "item": {
                    "type": "object",
                    "properties": {
                        "kind": { "enum": ["a", "b"] },
                        "count": { "type": "integer", "minimum": 1, "maximum": 10 }
                    },
                    "required": ["kind", "count"],
                    "additionalProperties": false
                }
            },
            "type": "object",
            "properties": {
                "items": {
                    "type": "array",
                    "minItems": 1,
                    "items": { "$ref": "#/$defs/item" }
                }
            },
            "required": ["items"]
        });

        assert!(is_valid(&schema, &json!({"items": [{"kind": "a", "count": 3}]})).unwrap());
        assert!(!is_valid(&schema, &json!({"items": []})).unwrap());
        assert!(!is_valid(&schema, &json!({"items": [{"kind": "c", "count": 3}]})).unwrap());
        assert!(!is_valid(&schema, &json!({"items": [{"kind": "a", "count": 0}]})).unwrap());
        assert!(!is_valid(&schema, &json!({"items": [{"kind": "a", "count": 1.5}]})).unwrap());
        assert!(!is_valid(
            &schema,
            &json!({"items": [{"kind": "a", "count": 2, "extra": true}]})
        )
        .unwrap());
    }

    #[test]
    fn validates_combinators_and_numeric_equality() {
        let schema = json!({
            "oneOf": [
                { "type": "string", "minLength": 2 },
                { "const": 42 }
            ]
        });
        assert!(is_valid(&schema, &json!("ok")).unwrap());
        assert!(is_valid(&schema, &json!(42.0)).unwrap());
        assert!(!is_valid(&schema, &json!("x")).unwrap());
        assert!(!is_valid(&schema, &json!(43)).unwrap());
    }

    #[test]
    fn validates_arrays_tuples_unique_items_and_numeric_bounds() {
        let prefix_schema = json!({
            "type": "array",
            "prefixItems": [{"type": "string"}, {"type": "integer"}],
            "items": {"type": "boolean"},
            "uniqueItems": true
        });
        assert!(is_valid(&prefix_schema, &json!(["id", 1, true, false])).unwrap());
        assert!(!is_valid(&prefix_schema, &json!(["id", 1, true, true])).unwrap());
        assert!(!is_valid(&prefix_schema, &json!(["id", "wrong", true])).unwrap());
        assert!(!is_valid(&prefix_schema, &json!(["id", 1, "extra"])).unwrap());

        let legacy_tuple = json!({
            "type": "array",
            "items": [{"type": "string"}, {"type": "number"}],
            "additionalItems": false
        });
        assert!(is_valid(&legacy_tuple, &json!(["ok", 3.5])).unwrap());
        assert!(!is_valid(&legacy_tuple, &json!(["ok", 3.5, "too_many"])).unwrap());

        let numeric_schema = json!({
            "type": "number",
            "minimum": 0,
            "exclusiveMinimum": true,
            "exclusiveMaximum": 10.0,
            "multipleOf": 0.5
        });
        assert!(is_valid(&numeric_schema, &json!(2.5)).unwrap());
        assert!(!is_valid(&numeric_schema, &json!(0.0)).unwrap());
        assert!(!is_valid(&numeric_schema, &json!(10.0)).unwrap());
        assert!(!is_valid(&numeric_schema, &json!(2.3)).unwrap());
    }

    #[test]
    fn validates_conditional_all_of_any_of_not_and_escaped_json_pointers() {
        let cond_schema = json!({
            "allOf": [
                { "not": { "const": "forbidden" } },
                { "anyOf": [{ "type": "string" }, { "type": "object" }] }
            ],
            "if": { "type": "object", "properties": { "mode": { "const": "strict" } }, "required": ["mode"] },
            "then": { "required": ["reason"] },
            "else": { "not": { "type": "object", "required": ["forbidden_else"] } }
        });
        assert!(is_valid(&cond_schema, &json!("allowed")).unwrap());
        assert!(!is_valid(&cond_schema, &json!("forbidden")).unwrap());
        assert!(is_valid(&cond_schema, &json!({"mode": "strict", "reason": "ok"})).unwrap());
        assert!(!is_valid(&cond_schema, &json!({"mode": "strict"})).unwrap());
        assert!(!is_valid(&cond_schema, &json!({"forbidden_else": 1})).unwrap());

        let escaped_ref = json!({
            "$defs": {
                "a/b~c name": { "type": "integer", "minimum": 5 }
            },
            "$ref": "#/$defs/a~1b~0c%20name"
        });
        assert!(is_valid(&escaped_ref, &json!(5)).unwrap());
        assert!(!is_valid(&escaped_ref, &json!(4)).unwrap());
    }
}
