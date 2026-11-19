pub mod app_server;
pub mod app_server_client;
pub mod launch;
pub mod ownership_lock;
pub mod registry;
pub mod version;
use serde_json::{json, Value};

pub fn parse(args: &[String]) -> Result<Value, String> {
    let mut out = json!({"flavor":"codex"});
    let mut pass = vec![];
    let mut explicit = false;
    let mut iter = args.iter().enumerate().peekable();
    while let Some((i, arg)) = iter.next() {
        match arg.as_str() {
            "resume" if i == 0 => {
                if let Some((_, id)) =
                    iter.next_if(|(_, id)| !id.is_empty() && !id.starts_with('-'))
                {
                    out["resumeSessionId"] = json!(id);
                } else if args.iter().any(|s| s == "--last") {
                    out["resumeLast"] = json!(true);
                } else {
                    return Err("Use hapi resume to choose a HAPI session, or hapi codex resume <native-id> / --last".into());
                }
            }
            "--last" if out["resumeLast"] == true => {}
            "--all" if out["resumeLast"] == true => out["resumeAll"] = json!(true),
            "--" => {
                pass.extend(&args[i..]);
                break;
            }
            "fork" | "exec" | "review" | "login" | "logout" if i == 0 => {
                return Err(format!(
                    "Use native codex {arg} outside HAPI, or /{arg} in an attached terminal"
                ))
            }
            "--started-by" => {
                if let Some((_, v)) = iter.next() {
                    out["startedBy"] = json!(v);
                } else {
                    out.as_object_mut().unwrap().remove("startedBy");
                }
            }
            "--yolo" | "--dangerously-bypass-approvals-and-sandbox" => {
                if !explicit {
                    out["permissionMode"] = json!("yolo");
                }
                pass.push(arg);
            }
            "--permission-mode" => {
                let v = iter.next().map_or("(missing)", |(_, s)| s.as_str());
                if !matches!(v, "default" | "read-only" | "autoReview" | "yolo") {
                    return Err(format!("Invalid --permission-mode value: {v}"));
                }
                explicit = true;
                out["permissionMode"] = json!(v);
            }
            "--hapi-session-id"
            | "--existing-session-id"
            | "--model"
            | "--model-reasoning-effort"
            | "--service-tier"
            | "--collaboration-mode" => {
                let v = iter
                    .next()
                    .map(|(_, s)| s)
                    .filter(|v| !v.is_empty())
                    .ok_or_else(|| format!("Missing {arg} value"))?;
                let normalized = v.trim().to_lowercase();
                match arg.as_str() {
                    "--hapi-session-id" => out["reservedSessionId"] = json!(v),
                    "--existing-session-id" => out["existingSessionId"] = json!(v),
                    "--model" => {
                        out["model"] = json!(v);
                        pass.extend([arg, v]);
                    }
                    "--model-reasoning-effort" if !normalized.is_empty() => {
                        out["modelReasoningEffort"] = json!(normalized)
                    }
                    "--model-reasoning-effort" => {
                        return Err("Invalid model reasoning effort".into())
                    }
                    "--service-tier" if matches!(normalized.as_str(), "fast" | "standard") => {
                        out["serviceTier"] = json!(normalized)
                    }
                    "--collaboration-mode" if matches!(normalized.as_str(), "plan" | "default") => {
                        out["collaborationMode"] = json!(normalized)
                    }
                    _ => return Err(format!("Invalid {arg} value")),
                }
            }
            _ => pass.push(arg),
        }
    }
    if !pass.is_empty() {
        out["codexArgs"] = json!(pass);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_codex_options() {
        let args="resume native --model m --model-reasoning-effort HIGH --service-tier Fast --collaboration-mode PLAN -c k=v --enable foo".split_whitespace().map(String::from).collect::<Vec<_>>();
        assert_eq!(
            parse(&args).unwrap(),
            json!({"flavor":"codex","resumeSessionId":"native","model":"m","modelReasoningEffort":"high","serviceTier":"fast","collaborationMode":"plan","codexArgs":["--model","m","-c","k=v","--enable","foo"]})
        );
    }
}
