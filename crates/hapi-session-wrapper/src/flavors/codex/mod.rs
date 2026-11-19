pub mod app_server;
pub mod app_server_client;
pub mod async_questions;
pub mod compaction_summary;
pub mod event_converter;
pub mod gateway;
pub mod generation_ledger;
pub mod launch;
pub mod ownership_lock;
pub mod permission_adapter;
pub mod permissions;
pub mod plan;
pub mod projection;
pub mod queue;
pub mod registry;
pub mod root;
pub mod round_duration;
pub mod settings;
pub mod slash_commands;
mod value;
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

#[cfg(test)]
mod model_tests;
#[cfg(test)]
mod utility_tests;

#[cfg(test)]
#[test]
fn argv_matches_typescript_command() {
    use std::{
        io::Write,
        process::{Command, Stdio},
    };
    let mut cases = vec![
        vec![],
        vec!["resume"],
        vec!["resume", "native"],
        vec!["resume", "--last", "--all"],
        vec!["--yolo", "--permission-mode", "read-only"],
        vec!["--permission-mode", "default", "--yolo"],
        vec!["--dangerously-bypass-approvals-and-sandbox"],
        vec!["--started-by"],
        vec!["--started-by", "runner"],
        vec![
            "--hapi-session-id",
            "reserved",
            "--existing-session-id",
            "existing",
        ],
        vec!["--", "--model", "literal", "--yolo"],
        vec!["-c", "a=b", "--enable", "flag"],
        vec![
            "--model",
            "model",
            "--model-reasoning-effort",
            " HIGH ",
            "--service-tier",
            "FAST",
            "--collaboration-mode",
            " Plan ",
        ],
    ];
    for sub in ["fork", "exec", "review", "login", "logout"] {
        cases.push(vec![sub]);
    }
    for flag in [
        "--model",
        "--model-reasoning-effort",
        "--service-tier",
        "--collaboration-mode",
        "--permission-mode",
        "--hapi-session-id",
        "--existing-session-id",
    ] {
        for value in [None, Some(""), Some(" "), Some("invalid")] {
            let mut case = vec![flag];
            case.extend(value);
            cases.push(case);
        }
    }
    let mut child = Command::new("bun")
        .current_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/../../cli"))
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/flavors/codex/argv_vectors.ts"
        ))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&serde_json::to_vec(&cases).unwrap())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    let expected: Vec<Value> = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(cases.len(), expected.len());
    for (args, want) in cases.into_iter().zip(expected) {
        let actual = parse(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
            .unwrap_or_else(Value::String);
        assert_eq!(actual, want, "{args:?}");
    }
}
