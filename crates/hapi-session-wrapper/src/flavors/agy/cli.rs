//! `hapi agy` argv (TS `commands/agy.ts` + `parseRemoteAgentCommandOptions`).
use serde_json::{json, Value};

pub const AGY_PERMISSION_MODES: [&str; 4] =
    ["request-review", "acceptEdits", "plan", "always-proceed"];

fn value(iter: &mut std::slice::Iter<'_, String>, missing: &str) -> Result<String, String> {
    iter.next()
        .filter(|v| !v.is_empty())
        .cloned()
        .ok_or_else(|| missing.to_owned())
}

pub fn parse(args: &[String]) -> Result<Value, String> {
    let mut out = json!({"flavor":"agy"});
    let mut explicit_mode = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--started-by" => match iter.next() {
                Some(v) => out["startedBy"] = json!(v),
                None => {
                    out.as_object_mut().unwrap().remove("startedBy");
                }
            },
            "--hapi-starting-mode" => {
                let v = iter.next().map(String::as_str);
                if v != Some("remote") {
                    return Err(
                        "Invalid --hapi-starting-mode (expected local, remote, or pty)".into(),
                    );
                }
                out["startingMode"] = json!("remote");
            }
            "--existing-session-id" => {
                let id = iter
                    .next()
                    .filter(|v| !v.is_empty() && !v.starts_with('-'))
                    .ok_or("Missing --existing-session-id value")?;
                out["existingSessionId"] = json!(id);
            }
            "--permission-mode" => {
                let mode = iter.next();
                match mode.filter(|m| AGY_PERMISSION_MODES.contains(&m.as_str())) {
                    Some(m) => out["permissionMode"] = json!(m),
                    None => {
                        return Err(format!(
                            "Invalid --permission-mode value: {}",
                            mode.map_or("(missing)", String::as_str)
                        ))
                    }
                }
                explicit_mode = true;
            }
            "--yolo" if !explicit_mode => out["permissionMode"] = json!("always-proceed"),
            "--hapi-session-id" => {
                out["reservedSessionId"] =
                    json!(value(&mut iter, "Missing --hapi-session-id value")?)
            }
            "--resume" => {
                out["resumeSessionId"] = json!(value(&mut iter, "Missing --resume value")?)
            }
            "-s" | "--session" => {
                out["resumeSessionId"] = json!(value(&mut iter, &format!("Missing {arg} value"))?)
            }
            "--session-id" => {
                out["resumeSessionId"] = json!(value(&mut iter, "Missing --session-id value")?)
            }
            "--model" => out["model"] = json!(value(&mut iter, "Missing --model value")?),
            "--effort" => out["effort"] = json!(value(&mut iter, "Missing --effort value")?),
            "--model-reasoning-effort" => {
                out["modelReasoningEffort"] =
                    json!(value(&mut iter, "Missing --model-reasoning-effort value")?)
            }
            _ => {}
        }
    }
    if out.get("startingMode").is_none() {
        out["startingMode"] = json!("remote");
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Result<Value, String> {
        parse(&s.split_whitespace().map(String::from).collect::<Vec<_>>())
    }

    #[test]
    fn parses_runner_argv() {
        assert_eq!(
            p("--hapi-starting-mode remote --started-by runner --hapi-session-id a1 --model gemini-4-argon --effort high --resume conv-1"),
            Ok(json!({"flavor":"agy","startingMode":"remote","startedBy":"runner","reservedSessionId":"a1","model":"gemini-4-argon","effort":"high","resumeSessionId":"conv-1"}))
        );
        assert_eq!(p(""), Ok(json!({"flavor":"agy","startingMode":"remote"})));
        assert_eq!(
            p("--existing-session-id s1 --permission-mode plan --yolo"),
            Ok(
                json!({"flavor":"agy","existingSessionId":"s1","permissionMode":"plan","startingMode":"remote"})
            )
        );
        assert_eq!(p("--yolo").unwrap()["permissionMode"], "always-proceed");
    }

    #[test]
    fn rejects_bad_values() {
        assert_eq!(
            p("--hapi-starting-mode local").unwrap_err(),
            "Invalid --hapi-starting-mode (expected local, remote, or pty)"
        );
        assert_eq!(
            p("--permission-mode yolo").unwrap_err(),
            "Invalid --permission-mode value: yolo"
        );
        assert_eq!(
            p("--permission-mode").unwrap_err(),
            "Invalid --permission-mode value: (missing)"
        );
        assert_eq!(
            p("--existing-session-id --model").unwrap_err(),
            "Missing --existing-session-id value"
        );
        assert_eq!(p("--model").unwrap_err(), "Missing --model value");
    }
}
