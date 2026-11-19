use super::value::*;
use serde_json::{json, Value};
pub fn matches(actual: &Value, expected: &Value) -> bool {
    expected.as_object().into_iter().flatten().all(|(k, v)| {
        k == "threadId"
            || k == "developer_instructions" && v.is_null()
            || if v.is_object() {
                matches(&actual[k], v)
            } else {
                actual.get(k) == Some(v)
            }
    })
}
pub fn inherited_sandbox(state: &Value) -> Result<Value, String> {
    if !matches!(
        &state["activePermissionProfile"],
        Value::Null | Value::Bool(false)
    ) {
        return Err("Create this conversation in the native TUI: named permission profile inheritance is not supported".into());
    }
    let s = field(state, "sandboxPolicy sandbox");
    match text(s, "type") {
        None => Ok(json!({})),
        Some("dangerFullAccess") => Ok(json!({"sandbox":"danger-full-access"})),
        Some("readOnly") => {
            if !s["access"].is_null() && s["access"]["type"] != "fullAccess" {
                Err("Cannot inherit a restricted read-only filesystem; create this conversation in the native TUI".into())
            } else {
                Ok(json!({"sandbox":"read-only"}))
            }
        }
        Some("workspaceWrite") => {
            let mut config = json!({});
            for (src, dst) in [
                ("writableRoots", "writable_roots"),
                ("networkAccess", "network_access"),
                ("excludeTmpdirEnvVar", "exclude_tmpdir_env_var"),
                ("excludeSlashTmp", "exclude_slash_tmp"),
            ] {
                if if src == "writableRoots" {
                    s[src].is_array()
                } else {
                    s[src].is_boolean()
                } {
                    config[format!("sandbox_workspace_write.{dst}")] = s[src].clone();
                }
            }
            Ok(json!({"sandbox":"workspace-write","config":config}))
        }
        Some(kind) => Err(format!(
            "Cannot inherit sandbox {kind}; create this conversation in the native TUI"
        )),
    }
}
