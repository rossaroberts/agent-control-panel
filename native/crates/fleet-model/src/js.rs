//! JavaScript value semantics over JSON shared by the ports of fleet/machine.mjs: `x ?? y`, `String(v)` and
//! `xs.join(sep)`. One copy, so `stageOf` and `stepOf` agree. Known gap: a float prints as Rust does (`1.0`), not JS (`1`).

use serde_json::Value;

/// `v ?? fallback` over JSON: absent and `null` fall through.
pub fn present(v: Option<&Value>) -> Option<&Value> {
    v.filter(|v| !v.is_null())
}

/// `String(v)` (and template interpolation) for JSON values.
pub fn string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Array(xs) => join(xs, ","),
        Value::Object(_) => "[object Object]".into(),
    }
}

/// `xs.join(sep)`: `null` elements are empty.
pub fn join(xs: &[Value], sep: &str) -> String {
    xs.iter()
        .map(|x| {
            if x.is_null() {
                String::new()
            } else {
                string(x)
            }
        })
        .collect::<Vec<_>>()
        .join(sep)
}
