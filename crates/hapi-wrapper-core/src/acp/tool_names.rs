//! Tool naming helpers (TS `cli/src/agent/utils.ts`).
use super::types::PermissionRequest;
use serde_json::{json, Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolNameSource {
    Title,
    RawInputName,
    RawInputTool,
    Kind,
    Default,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DerivedToolName {
    pub name: String,
    pub source: ToolNameSource,
}

const EDIT_KINDS: &[&str] = &[
    "edit",
    "write",
    "write_file",
    "replace",
    "modify",
    "file_edit",
];

/// The request's input: `rawInput`, else `rawOutput`, else null.
pub fn derive_tool_input(request: &PermissionRequest) -> Value {
    request
        .raw_input
        .clone()
        .or_else(|| request.raw_output.clone().filter(|v| !v.is_null()))
        .unwrap_or(Value::Null)
}

fn first_string<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a str> {
    let object = value.as_object()?;
    keys.iter()
        .find_map(|k| object.get(*k).and_then(Value::as_str))
}

/// Normalizes OpenCode-native edit/write inputs to the Claude-shaped
/// `Edit`/`Write` inputs; `None` unless the semantic hint is an edit alias.
pub fn canonicalize_diff_tool_input(raw: &Value, hint: Option<&str>) -> Option<(String, Value)> {
    let kind = hint?.trim().to_lowercase();
    if !EDIT_KINDS.contains(&kind.as_str()) {
        return None;
    }
    let path = first_string(raw, &["filePath", "file_path"]).filter(|p| !p.is_empty())?;
    let old = first_string(raw, &["oldString", "old_string"]);
    let new = first_string(raw, &["newString", "new_string"]);
    if let (Some(old), Some(new)) = (old, new) {
        let mut input = json!({"file_path":path,"old_string":old,"new_string":new});
        if let Some(all) = raw["replaceAll"]
            .as_bool()
            .or_else(|| raw["replace_all"].as_bool())
        {
            input["replace_all"] = all.into();
        }
        return Some(("Edit".into(), input));
    }
    let content = first_string(raw, &["content"])?;
    Some(("Write".into(), json!({"file_path":path,"content":content})))
}

fn normalize_name(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

pub fn is_placeholder_tool_name(name: &str) -> bool {
    matches!(
        name.trim().to_lowercase().as_str(),
        "" | "tool" | "unknown" | "other"
    )
}

pub fn derive_tool_name_with_source(
    title: Option<&str>,
    kind: Option<&str>,
    raw_input: Option<&Value>,
    meta_kind: Option<&str>,
) -> DerivedToolName {
    let pick = |name: String, source| DerivedToolName { name, source };
    if let Some(t) = normalize_name(title) {
        return pick(t, ToolNameSource::Title);
    }
    if let Some(raw) = raw_input.filter(|v| v.is_object()) {
        if let Some(n) = normalize_name(raw["name"].as_str()) {
            return pick(n, ToolNameSource::RawInputName);
        }
        if let Some(n) = normalize_name(raw["tool"].as_str()) {
            return pick(n, ToolNameSource::RawInputTool);
        }
    }
    let lower = kind.map(|k| k.trim().to_lowercase());
    if lower.as_deref().is_some_and(|k| EDIT_KINDS.contains(&k)) {
        match meta_kind {
            Some("add") => return pick("Write".into(), ToolNameSource::Kind),
            Some("modify") => return pick("Edit".into(), ToolNameSource::Kind),
            _ => {}
        }
    }
    if let Some(k) = normalize_name(kind).filter(|k| !is_placeholder_tool_name(k)) {
        return pick(k, ToolNameSource::Kind);
    }
    pick("Tool".into(), ToolNameSource::Default)
}

pub fn derive_tool_name(
    title: Option<&str>,
    kind: Option<&str>,
    raw_input: Option<&Value>,
) -> String {
    derive_tool_name_with_source(title, kind, raw_input, None).name
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        let d = |t, k, r: Option<&Value>, m| derive_tool_name_with_source(t, k, r, m);
        assert_eq!(d(Some(" ls "), None, None, None).name, "ls");
        assert_eq!(
            d(None, Some("edit"), Some(&json!({"name":"x"})), None).source,
            ToolNameSource::RawInputName
        );
        assert_eq!(d(None, None, Some(&json!({"tool":"t"})), None).name, "t");
        assert_eq!(d(None, Some("write"), None, Some("add")).name, "Write");
        assert_eq!(d(None, Some("replace"), None, Some("modify")).name, "Edit");
        assert_eq!(d(None, Some("execute"), None, None).name, "execute");
        assert_eq!(d(None, Some("other"), None, None).name, "Tool");
        assert!(is_placeholder_tool_name(" Unknown "));
    }

    #[test]
    fn canonical_diffs() {
        let raw = json!({"filePath":"/a","oldString":"x","newString":"y","replaceAll":true});
        assert_eq!(
            canonicalize_diff_tool_input(&raw, Some("edit")),
            Some((
                "Edit".into(),
                json!({"file_path":"/a","old_string":"x","new_string":"y","replace_all":true})
            ))
        );
        let raw = json!({"filePath":"/a","content":"c"});
        let write = canonicalize_diff_tool_input(&raw, Some("write")).unwrap();
        assert_eq!(write.0, "Write");
        assert_eq!(canonicalize_diff_tool_input(&raw, Some("mcp")), None);
        assert_eq!(canonicalize_diff_tool_input(&raw, None), None);
    }
}
