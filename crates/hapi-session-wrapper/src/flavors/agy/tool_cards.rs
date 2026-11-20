//! agy tool names and args to the canonical web cards (TS `agyToolCards.ts`).
use hapi_wrapper_core::acp::{AgentMessage, ToolCall};
use serde_json::{json, Map, Value};

/// Shown for a command agy moved to the background until its real result arrives.
pub fn background_command_result() -> Value {
    json!({"stdout":"Running in the background. The result appears when agy reports it."})
}

fn text(v: &Value) -> Option<&str> {
    v.as_str().filter(|s| !s.is_empty())
}
fn compact_keeping(pairs: Vec<(&str, Value)>, keep: &[&str]) -> Value {
    let mut out = Map::new();
    for (k, v) in pairs {
        if v.is_null() || (v == "" && !keep.contains(&k)) {
            continue;
        }
        out.insert(k.into(), v);
    }
    Value::Object(out)
}
fn compact(pairs: Vec<(&str, Value)>) -> Value {
    compact_keeping(pairs, &[])
}
fn finite_number(v: &Value) -> Option<String> {
    let f = v.as_f64().filter(|f| f.is_finite())?;
    Some(match (v.as_i64(), v.as_u64()) {
        (Some(i), _) => i.to_string(),
        (_, Some(u)) => u.to_string(),
        _ => f.to_string(),
    })
}
fn parse_arguments(v: &Value) -> Value {
    let parsed = match v {
        Value::String(s) => serde_json::from_str::<Value>(s).unwrap_or(Value::Null),
        other => other.clone(),
    };
    if parsed.is_object() {
        parsed
    } else {
        json!({})
    }
}
/// An artifact write (plan, task list) lives in agy's private brain directory.
fn artifact_plan(args: &Value) -> Option<String> {
    let metadata = args.get("ArtifactMetadata").filter(|m| m.is_object())?;
    let content = text(&args["Content"]).unwrap_or("");
    match text(&metadata["Summary"]) {
        Some(summary) if !content.trim_start().starts_with('#') => {
            Some(format!("# {summary}\n\n{content}"))
        }
        _ => Some(content.into()),
    }
}

pub fn map_agy_tool_call(message: &ToolCall) -> ToolCall {
    let Some(native) = message.native_name.as_deref() else {
        return message.clone();
    };
    let Some(args) = message.native_args.as_ref().filter(|a| a.is_object()) else {
        return message.clone();
    };
    let a = |k: &str| args[k].clone();
    let with = |name: &str, input: Value| ToolCall {
        name: name.into(),
        input,
        ..message.clone()
    };
    let (name, input) = match native {
        "run_command" => (
            "Bash".to_owned(),
            compact(vec![
                ("command", a("CommandLine")),
                ("cwd", a("Cwd")),
                ("description", a("toolSummary")),
            ]),
        ),
        "read_file" => ("Read".into(), compact(vec![("file_path", a("Path"))])),
        "edit_file" => (
            "Edit".into(),
            compact_keeping(
                vec![
                    ("file_path", a("Path")),
                    ("old_string", a("Target")),
                    ("new_string", a("Replacement")),
                ],
                &["old_string", "new_string"],
            ),
        ),
        "write_file" => match artifact_plan(args) {
            Some(plan) => ("ExitPlanMode".into(), json!({"plan":plan})),
            None => (
                "Write".into(),
                compact_keeping(
                    vec![("file_path", a("Path")), ("content", a("Content"))],
                    &["content"],
                ),
            ),
        },
        "search_web" => ("WebSearch".into(), compact(vec![("query", a("query"))])),
        "invoke_subagent" => (
            "Task".into(),
            compact(vec![
                ("description", a("Role")),
                ("prompt", a("Prompt")),
                ("subagent_type", a("TypeName")),
            ]),
        ),
        "swarm" => match text(&args["action"]) {
            Some("add") => {
                let description = match text(&args["name"]) {
                    Some(w) => format!("Swarm worker: @{w}"),
                    None => "Swarm worker".into(),
                };
                (
                    "Task".into(),
                    compact(vec![
                        ("description", json!(description)),
                        ("prompt", a("brief")),
                        ("subagent_type", a("model")),
                    ]),
                )
            }
            Some("title") => (
                "TeamCreate".into(),
                compact(vec![("team_name", a("title"))]),
            ),
            Some("kill") => ("close_agent".into(), compact(vec![("target", a("name"))])),
            Some(action @ ("status" | "models")) => (
                "list_agents".into(),
                compact(vec![("path_prefix", json!(action))]),
            ),
            Some("model") => {
                let target = text(&args["name"]).unwrap_or("coordinator");
                let mut out = with(
                    "swarm",
                    compact(vec![("name", json!(text(&args["model"])))]),
                );
                out.title = Some(format!("Switch @{target} model"));
                return out;
            }
            _ => return message.clone(),
        },
        "post_message" => (
            "SendMessage".into(),
            compact(vec![
                (
                    "recipient",
                    json!(text(&args["channel"])
                        .map_or("swarm board".to_owned(), |c| format!("#{c}"))),
                ),
                ("summary", a("text")),
            ]),
        ),
        "send_message" => (
            "SendMessage".into(),
            compact(vec![
                ("recipient", a("Recipient")),
                ("summary", a("Message")),
            ]),
        ),
        "swarm_wait" => {
            let target = finite_number(&args["since"])
                .map_or("swarm board".to_owned(), |n| format!("board > #{n}"));
            (
                "wait_agent".into(),
                compact(vec![("target", json!(target))]),
            )
        }
        "read_board" => {
            let channel = text(&args["channel"]);
            let since = finite_number(&args["since"]).map(|n| format!("since #{n}"));
            let mut out = with(
                "read_board",
                compact(vec![(
                    "name",
                    json!(since.or_else(|| channel.map(str::to_owned))),
                )]),
            );
            out.title = Some(match channel {
                Some(c) => format!("Read swarm board (#{c})"),
                None => "Read swarm board".into(),
            });
            return out;
        }
        "call_mcp_tool" => {
            let (Some(server), Some(tool)) = (text(&args["ServerName"]), text(&args["ToolName"]))
            else {
                return message.clone();
            };
            (
                format!("mcp__{server}__{tool}"),
                parse_arguments(&args["Arguments"]),
            )
        }
        _ => return message.clone(),
    };
    let mut out = with(&name, input);
    if name == "ExitPlanMode" {
        // An artifact plan must not leak the brain path through the title.
        out.title = None;
    }
    out
}

/// agy frames a command result as "Command exited with code N.\n\nCommand output:\n<text>".
pub fn map_agy_tool_result(message: &AgentMessage, native_name: Option<&str>) -> AgentMessage {
    let AgentMessage::ToolResult { id, output, status } = message else {
        return message.clone();
    };
    if native_name != Some("run_command") {
        return message.clone();
    }
    let Some(body) = output.get("output").and_then(text) else {
        return message.clone();
    };
    let body = body.replace("\r\n", "\n");
    let framing =
        regex_lite::Regex::new(r"^Command exited with code -?\d+\.\s*\n\s*Command output:\n?")
            .unwrap();
    let body = framing.replace(&body, "").trim_end().to_owned();
    let mut next = json!({"stdout":body});
    if output["exit_code"].is_number() {
        next["exit_code"] = output["exit_code"].clone();
    }
    AgentMessage::ToolResult {
        id: id.clone(),
        output: next,
        status: status.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(native: &str, args: Value) -> ToolCall {
        ToolCall {
            id: "call_1".into(),
            name: "some title".into(),
            input: args.clone(),
            status: "in_progress".into(),
            title: Some("some title".into()),
            kind: Some("other".into()),
            progress: None,
            native_name: Some(native.into()),
            native_args: Some(args),
        }
    }
    fn mapped(native: &str, args: Value) -> (String, Value) {
        let m = map_agy_tool_call(&call(native, args));
        (m.name, m.input)
    }

    #[test]
    fn maps_core_tools() {
        assert_eq!(
            mapped(
                "run_command",
                json!({"CommandLine":"ls -la","Cwd":"/tmp/x","WaitMsBeforeAsync":5000,"toolSummary":"List files"})
            ),
            (
                "Bash".into(),
                json!({"command":"ls -la","cwd":"/tmp/x","description":"List files"})
            )
        );
        assert_eq!(
            mapped("read_file", json!({"Path":"/a/b.md"})),
            ("Read".into(), json!({"file_path":"/a/b.md"}))
        );
        assert_eq!(
            mapped(
                "edit_file",
                json!({"Path":"/a","Target":"foo","Replacement":""})
            ),
            (
                "Edit".into(),
                json!({"file_path":"/a","old_string":"foo","new_string":""})
            )
        );
        assert_eq!(
            mapped("write_file", json!({"Path":"/a","Content":""})),
            ("Write".into(), json!({"file_path":"/a","content":""}))
        );
        assert_eq!(
            mapped("search_web", json!({"query":"acp"})),
            ("WebSearch".into(), json!({"query":"acp"}))
        );
        assert_eq!(
            mapped(
                "invoke_subagent",
                json!({"Prompt":"Draw it","Role":"Image Generator","TypeName":"image-generator","Model":"inherit"})
            ),
            (
                "Task".into(),
                json!({"description":"Image Generator","prompt":"Draw it","subagent_type":"image-generator"})
            )
        );
    }

    #[test]
    fn artifact_write_is_a_plan_without_brain_path() {
        let mut c = call(
            "write_file",
            json!({"Path":"/home/u/.gemini/antigravity-cli/brain/abc/plan.md","Content":"- [ ] step 1\n","ArtifactMetadata":{"Summary":"Haiku Plan"}}),
        );
        c.title = Some("File ../brain/abc/plan.md".into());
        let m = map_agy_tool_call(&c);
        assert_eq!(m.name, "ExitPlanMode");
        assert_eq!(m.input, json!({"plan":"# Haiku Plan\n\n- [ ] step 1\n"}));
        assert_eq!(m.title, None);
        assert_eq!(
            mapped("write_file", json!({"Path":"/x/brain/p.md","Content":"# Own title\n\nbody","ArtifactMetadata":{"Summary":"Other"}})).1,
            json!({"plan":"# Own title\n\nbody"})
        );
    }

    #[test]
    fn maps_swarm_tools() {
        assert_eq!(
            mapped(
                "swarm",
                json!({"action":"add","name":"parser","brief":"Fix","model":"m"})
            ),
            (
                "Task".into(),
                json!({"description":"Swarm worker: @parser","prompt":"Fix","subagent_type":"m"})
            )
        );
        assert_eq!(
            mapped("swarm", json!({"action":"title","title":"t"})),
            ("TeamCreate".into(), json!({"team_name":"t"}))
        );
        assert_eq!(
            mapped("swarm", json!({"action":"kill","name":"p"})),
            ("close_agent".into(), json!({"target":"p"}))
        );
        assert_eq!(
            mapped("swarm", json!({"action":"status"})),
            ("list_agents".into(), json!({"path_prefix":"status"}))
        );
        let m = map_agy_tool_call(&call(
            "swarm",
            json!({"action":"model","name":"parser","model":"g"}),
        ));
        assert_eq!(
            (m.name.as_str(), m.title.as_deref()),
            ("swarm", Some("Switch @parser model"))
        );
        assert_eq!(m.input, json!({"name":"g"}));
        assert_eq!(
            mapped("post_message", json!({"text":"hi","channel":"parser"})),
            (
                "SendMessage".into(),
                json!({"recipient":"#parser","summary":"hi"})
            )
        );
        assert_eq!(
            mapped(
                "send_message",
                json!({"Recipient":"conv-123","Message":"Done"})
            ),
            (
                "SendMessage".into(),
                json!({"recipient":"conv-123","summary":"Done"})
            )
        );
        assert_eq!(
            mapped("swarm_wait", json!({"since":4})),
            ("wait_agent".into(), json!({"target":"board > #4"}))
        );
        let m = map_agy_tool_call(&call("read_board", json!({"since":2,"channel":"parser"})));
        assert_eq!(m.title.as_deref(), Some("Read swarm board (#parser)"));
        assert_eq!(m.input, json!({"name":"since #2"}));
    }

    #[test]
    fn maps_mcp_tools_and_leaves_unknown_alone() {
        assert_eq!(
            mapped(
                "call_mcp_tool",
                json!({"ServerName":"hapi","ToolName":"change_title","Arguments":{"title":"T"}})
            ),
            ("mcp__hapi__change_title".into(), json!({"title":"T"}))
        );
        assert_eq!(
            mapped("call_mcp_tool", json!({"ServerName":"hapi","ToolName":"change_title","Arguments":"{\"title\":\"S\"}"})).1,
            json!({"title":"S"})
        );
        for c in [
            call("manage_task", json!({"a":1})),
            call("ask_question", json!({"questions":[]})),
        ] {
            assert_eq!(map_agy_tool_call(&c), c);
        }
        let mut plain = call("x", json!({}));
        plain.native_name = None;
        assert_eq!(map_agy_tool_call(&plain), plain);
        let mut c = call("read_file", json!({"Path":"/a"}));
        c.status = "pending".into();
        let m = map_agy_tool_call(&c);
        assert_eq!(
            (m.id.as_str(), m.status.as_str(), m.title.as_deref()),
            ("call_1", "pending", Some("some title"))
        );
    }

    #[test]
    fn strips_run_command_framing() {
        let r = AgentMessage::ToolResult {
            id: "c".into(),
            status: "completed".into(),
            output: json!({"output":"Command exited with code 0.\n\nCommand output:\nhello\r\nworld\r\n","exit_code":0}),
        };
        let AgentMessage::ToolResult { output, .. } = map_agy_tool_result(&r, Some("run_command"))
        else {
            panic!()
        };
        assert_eq!(output, json!({"stdout":"hello\nworld","exit_code":0}));
        let other = AgentMessage::ToolResult {
            id: "c".into(),
            status: "completed".into(),
            output: json!({"a":1}),
        };
        assert_eq!(map_agy_tool_result(&other, Some("read_file")), other);
        assert_eq!(map_agy_tool_result(&other, None), other);
        assert!(background_command_result()["stdout"]
            .as_str()
            .unwrap()
            .contains("background"));
    }
}
