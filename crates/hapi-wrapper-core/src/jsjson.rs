use serde::de::DeserializeOwned;
use serde_json::{Map, Value};

const MAX_DEPTH: usize = 320;

fn checked_depth(bytes: &[u8]) -> serde_json::Result<usize> {
    let mut depth = 0;
    let mut max_depth = 0;
    let mut in_string = false;
    let mut bytes = bytes.iter().copied();
    while let Some(byte) = bytes.next() {
        match byte {
            b'\\' if in_string => {
                let _ = bytes.next();
            }
            b'"' => in_string = !in_string,
            b'[' | b'{' if !in_string => {
                depth += 1;
                if depth > MAX_DEPTH {
                    return Err(serde::de::Error::custom(format_args!(
                        "JSON nesting exceeds {MAX_DEPTH} containers"
                    )));
                }
                max_depth = max_depth.max(depth);
            }
            b']' | b'}' if !in_string => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    Ok(max_depth)
}

fn deserialize<T: DeserializeOwned>(bytes: &[u8]) -> serde_json::Result<T> {
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    deserializer.disable_recursion_limit();
    let value = T::deserialize(&mut deserializer)?;
    deserializer.end()?;
    Ok(value)
}

fn parse_with<T: DeserializeOwned>(bytes: &[u8], repair_surrogates: bool) -> serde_json::Result<T> {
    let depth = checked_depth(bytes)?;
    let read = || match deserialize(bytes) {
        Err(error) if repair_surrogates => deserialize(&normalize_surrogates(bytes).ok_or(error)?),
        result => result,
    };
    if depth >= 128 {
        stacker::maybe_grow(1536 * 1024, 2 * 1024 * 1024, read)
    } else {
        read()
    }
}

/// Parse strict JSON with at most 320 nested arrays or objects per document.
pub fn parse<T: DeserializeOwned>(bytes: &[u8]) -> serde_json::Result<T> {
    parse_with(bytes, false)
}

pub(crate) fn decode<T: DeserializeOwned>(bytes: &[u8]) -> serde_json::Result<T> {
    parse_with(bytes, true)
}

pub fn merge(
    base: &Map<String, Value>,
    fields: impl IntoIterator<Item = (String, Option<Value>)>,
) -> Map<String, Value> {
    let mut result = base.clone();
    for (name, value) in fields {
        if let Some(value) = value {
            result.insert(name, value);
        } else {
            result.shift_remove(&name);
        }
    }
    result
}

pub fn deep_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(a), Value::Number(b)) => a.as_f64() == b.as_f64(),
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(a, b)| deep_equal(a, b))
        }
        (Value::Object(a), Value::Object(b)) => {
            a.len() == b.len()
                && a.iter()
                    .all(|(k, v)| b.get(k).is_some_and(|other| deep_equal(v, other)))
        }
        _ => a == b,
    }
}

pub fn number_string(n: f64) -> String {
    if !n.is_finite() {
        return "null".into();
    }
    if n == 0.0 {
        return "0".into();
    }
    let shortest = Value::from(n.abs()).to_string();
    let (mantissa, exponent) = shortest.split_once(['e', 'E']).unwrap_or((&shortest, "0"));
    let decimal = mantissa.find('.').unwrap_or(mantissa.len());
    let raw = mantissa.replace('.', "");
    let significant = raw.trim_start_matches('0');
    let digits = significant.trim_end_matches('0');
    let exp = exponent.parse::<i32>().expect("formatted exponent") + decimal as i32
        - (raw.len() - significant.len()) as i32
        - 1;
    let sign = if n < 0.0 { "-" } else { "" };
    if !(-6..21).contains(&exp) {
        let fraction = if digits.len() > 1 {
            format!(".{}", &digits[1..])
        } else {
            String::new()
        };
        return format!("{sign}{}{fraction}e{exp:+}", &digits[..1]);
    }
    if exp < 0 {
        return format!("{sign}0.{}{digits}", "0".repeat((-exp - 1) as usize));
    }
    let point = exp as usize + 1;
    if point >= digits.len() {
        return format!("{sign}{digits}{}", "0".repeat(point - digits.len()));
    }
    format!("{sign}{}.{}", &digits[..point], &digits[point..])
}

pub fn stringify(value: &Value) -> String {
    match value {
        Value::Number(n) => number_string(n.as_f64().expect("finite JSON number")),
        Value::Array(a) => format!(
            "[{}]",
            a.iter().map(stringify).collect::<Vec<_>>().join(",")
        ),
        Value::Object(o) => {
            let mut entries: Vec<_> = o.iter().collect();
            entries.sort_by_key(|(key, _)| {
                key.parse::<u32>()
                    .ok()
                    .filter(|n| n.to_string() == key.as_str())
                    .unwrap_or(u32::MAX)
            });
            let fields = entries
                .into_iter()
                .map(|(k, v)| format!("{}:{}", Value::String(k.clone()), stringify(v)))
                .collect::<Vec<_>>();
            format!("{{{}}}", fields.join(","))
        }
        _ => value.to_string(),
    }
}

fn unicode_escape(input: &[u8]) -> Option<u16> {
    let digits = input.strip_prefix(br"\u")?.get(..4)?;
    if !digits.iter().all(u8::is_ascii_hexdigit) {
        return None;
    }
    u16::from_str_radix(std::str::from_utf8(digits).ok()?, 16).ok()
}

fn normalize_surrogates(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut normalized = None;
    let mut pos = 0;
    while let Some(offset) = bytes[pos..].iter().position(|&b| b == b'\\') {
        let start = pos + offset;
        let Some(unit) = unicode_escape(&bytes[start..]) else {
            pos = (start + 2).min(bytes.len());
            continue;
        };
        pos = start + 6;
        if (0xd800..=0xdbff).contains(&unit)
            && unicode_escape(&bytes[pos..]).is_some_and(|low| (0xdc00..=0xdfff).contains(&low))
        {
            pos += 6;
        } else if (0xd800..=0xdfff).contains(&unit) {
            normalized.get_or_insert_with(|| bytes.to_vec())[start + 2..pos]
                .copy_from_slice(b"fffd");
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn patches_and_equality() {
        let base = json!({"a":1,"b":1,"nested":{"a":1}});
        let merged = merge(
            base.as_object().unwrap(),
            [
                ("a".into(), None),
                ("b".into(), Some(Value::Null)),
                ("nested".into(), Some(json!({"b":2}))),
            ],
        );
        assert!(deep_equal(
            &Value::Object(merged),
            &json!({"nested":{"b":2.0},"b":null})
        ));
        assert!(!deep_equal(&json!([1, 2]), &json!([2, 1])));
    }
    #[test]
    fn ecmascript_format() {
        assert_eq!(
            stringify(&json!({"x": [-0.0,1e-7,1e-6,1e20,1e21],"10":0,"2":1})),
            r#"{"2":1,"10":0,"x":[0,1e-7,0.000001,100000000000000000000,1e+21]}"#
        );
    }

    #[test]
    fn json_depth_bound_covers_the_value_lifecycle() {
        std::thread::Builder::new()
            .stack_size(1024 * 1024)
            .spawn(|| {
                for (open, close) in [("[", "]"), (r#"{"next":"#, "}")] {
                    let input = format!("{}0{}", open.repeat(MAX_DEPTH), close.repeat(MAX_DEPTH));
                    let value: Value = parse(input.as_bytes()).unwrap();
                    let cloned = value.clone();
                    assert!(deep_equal(&value, &cloned));
                    assert_eq!(serde_json::to_vec(&cloned).unwrap(), input.as_bytes());
                    assert_eq!(cloned.to_string(), input);
                    assert_eq!(stringify(&cloned), input);
                    let wrapped = json!({"tool_input": cloned});
                    assert_eq!(wrapped["tool_input"].to_string(), input);
                    drop(wrapped);
                    drop(cloned);
                    drop(value);

                    let too_deep = format!("{open}{input}{close}");
                    assert!(parse::<Value>(too_deep.as_bytes()).unwrap_err().is_data());
                    assert!(decode::<Value>(too_deep.as_bytes()).unwrap_err().is_data());
                    assert!(parse::<Value>(&input.as_bytes()[..input.len() - 1]).is_err());
                    assert!(parse::<Value>(format!("{input} 0").as_bytes()).is_err());
                }
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn json_depth_counts_containers_outside_strings() {
        let text = format!(
            r#"a quote " then {} and a backslash \"#,
            "{[".repeat(MAX_DEPTH)
        );
        let branch = format!(
            "{}0{}",
            "[".repeat(MAX_DEPTH - 1),
            "]".repeat(MAX_DEPTH - 1)
        );
        let input = format!(
            "[{},{branch},{branch}]",
            serde_json::to_string(&text).unwrap()
        );
        let value: Value = parse(input.as_bytes()).unwrap();
        assert_eq!(value[0], text);
        assert_eq!(value[1].to_string(), branch);
        assert_eq!(value[2].to_string(), branch);
    }
}
