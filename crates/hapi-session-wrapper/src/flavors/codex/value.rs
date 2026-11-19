use serde_json::{json, Value};
pub(super) fn field<'a>(v: &'a Value, keys: &str) -> &'a Value {
    keys.split_whitespace()
        .map(|k| &v[k])
        .find(|v| !v.is_null())
        .unwrap_or(&Value::Null)
}
pub(super) fn text<'a>(v: &'a Value, keys: &str) -> Option<&'a str> {
    field(v, keys).as_str().filter(|s| !s.is_empty())
}
pub(super) fn merge(mut a: Value, b: &Value) -> Value {
    if let Some(fields) = b.as_object() {
        for (k, v) in fields {
            a[k] = v.clone();
        }
    }
    a
}
pub(super) fn strings(v: &Value) -> Vec<&str> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|s| !s.is_empty())
        .collect()
}
pub(super) fn normalize(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-' && *c != '_')
        .collect()
}
pub(super) fn copy_fields(dst: &mut Value, v: &Value, fields: &[(&str, &str)]) {
    for &(k, source) in fields {
        let x = field(v, source);
        if !x.is_null() {
            dst[k] = x.clone();
        }
    }
}
pub(super) fn object(v: &Value) -> Value {
    if v.is_object() {
        v.clone()
    } else {
        json!({})
    }
}

pub(super) fn present<'de, D: serde::Deserializer<'de>, T: serde::Deserialize<'de>>(
    d: D,
) -> Result<Option<T>, D::Error> {
    T::deserialize(d).map(Some)
}
