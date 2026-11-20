use serde_json::{json, Value};

pub fn parse(args: &[String]) -> Result<Value, String> {
    let mut out = json!({"flavor": "claude"});
    let mut passthrough = Vec::new();
    let mut explicit = false;
    let mut args = args.iter().peekable();
    while let Some(arg) = args.next() {
        let field = match arg.as_str() {
            "--" => {
                passthrough.push(arg);
                passthrough.extend(args);
                break;
            }
            "--yolo" | "--dangerously-skip-permissions" => {
                if !explicit {
                    out["permissionMode"] = json!("bypassPermissions");
                }
                continue;
            }
            "--model" => "model",
            "--effort" => "effort",
            "--permission-mode" => "permissionMode",
            "--hapi-starting-mode" => "startingMode",
            "--existing-session-id" => "existingSessionId",
            "--hapi-session-id" => "reservedSessionId",
            "--started-by" => {
                if let Some(v) = args.next() {
                    out["startedBy"] = json!(v);
                } else {
                    out.as_object_mut().unwrap().remove("startedBy");
                }
                continue;
            }
            _ if arg.starts_with("--model=") => {
                let model = &arg[8..];
                if model.is_empty() {
                    return Err("Missing --model value".into());
                }
                out["model"] = json!(model);
                continue;
            }
            _ => {
                passthrough.push(arg);
                if let Some(v) = args.next_if(|v| !v.starts_with('-')) {
                    if arg == "--resume" {
                        out["resumeSessionId"] = json!(v);
                    }
                    passthrough.push(v);
                }
                continue;
            }
        };
        let value = args
            .next()
            .filter(|v| !v.is_empty())
            .ok_or_else(|| format!("Missing {arg} value"))?;
        if field == "permissionMode" {
            if !matches!(
                value.as_str(),
                "default" | "acceptEdits" | "bypassPermissions" | "plan" | "auto"
            ) {
                return Err(format!("Invalid {arg} value: {value}"));
            }
            explicit = true;
        }
        if field == "startingMode" && !matches!(value.as_str(), "local" | "remote") {
            return Err(format!("Invalid {arg} value: {value}"));
        }
        if field == "effort" {
            passthrough.extend([arg, value]);
        }
        out[field] = json!(value);
    }
    if !passthrough.is_empty() {
        out["claudeArgs"] = json!(passthrough);
    }
    Ok(out)
}
