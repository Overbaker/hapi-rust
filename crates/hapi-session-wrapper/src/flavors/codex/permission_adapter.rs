use super::value::*;
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::http::Uri;
use uuid::Uuid;

pub enum Action {
    Ignore,
    Immediate(Value),
    Ask(Prompt),
}
pub struct Prompt {
    pub id: String,
    pub tool: String,
    pub input: Value,
    kind: Kind,
}
enum Kind {
    Approval,
    Grant(Value),
    User,
    Tool(bool),
    Form(Value),
}
impl Prompt {
    pub fn is_question(&self) -> bool {
        matches!(self.kind, Kind::User | Kind::Form(_))
    }
    pub fn response(&self, decision: &str, answers: &Value) -> Value {
        let approved = matches!(decision, "approved" | "approved_for_session" | "accept");
        let session = decision == "approved_for_session";
        let declined = matches!(decision, "denied" | "decline");
        match &self.kind {
            Kind::Approval => {
                json!({"decision":if session{"acceptForSession"}else if approved{"accept"}else if declined{"decline"}else{"cancel"}})
            }
            Kind::Grant(req) => {
                json!({"permissions":if approved{req.clone()}else{json!({"network":null,"fileSystem":null})},"scope":if session{"session"}else{"turn"}})
            }
            Kind::User if approved => json!({"decision":"accept","answers":answers}),
            Kind::User => json!({"decision":if declined{"decline"}else{"cancel"}}),
            Kind::Tool(persist) => {
                json!({"action":if approved{"accept"}else if declined{"decline"}else{"cancel"},"content":null,"_meta":if session&&*persist{json!({"persist":"session"})}else{Value::Null}})
            }
            Kind::Form(request) => {
                json!({"action":if approved{"accept"}else if declined{"decline"}else{"cancel"},"content":if approved{form_content(request,answers)}else{Value::Null},"_meta":null})
            }
        }
    }
}
fn choices(p: &Value) -> Vec<Value> {
    for (v, array) in [(p, false), (&p["items"], true)] {
        if array && p["type"] != "array" {
            continue;
        }
        if let Some(list) = v["enum"].as_array() {
            return list.clone();
        }
        if let Some(list) = v["oneOf"].as_array() {
            return list
                .iter()
                .filter_map(|e| e.get("const").cloned())
                .collect();
        }
        if v["type"] == "boolean" {
            return vec![json!(true), json!(false)];
        }
    }
    vec![]
}
fn first(list: &Value, key: Option<&str>) -> Option<String> {
    list.as_array()?
        .iter()
        .find_map(|v| key.map_or(v, |k| &v[k]).as_str().map(str::to_string))
}
fn defaults(req: &Value) -> Value {
    let schema = &req["requestedSchema"];
    let mut out = json!({});
    for (k, p) in schema["properties"].as_object().into_iter().flatten() {
        if schema["required"]
            .as_array()
            .is_some_and(|a| !a.iter().any(|v| v == k))
            || !p.is_object()
        {
            continue;
        }
        out[k] = p
            .get("default")
            .cloned()
            .unwrap_or_else(|| match text(p, "type") {
                Some("string") => json!(first(&p["enum"], None)
                    .or_else(|| first(&p["oneOf"], Some("const")))
                    .unwrap_or_default()),
                Some("boolean") => json!(true),
                Some("integer" | "number") => json!(0),
                Some("array") => json!(first(&p["items"]["enum"], None)
                    .or_else(|| first(&p["items"]["anyOf"], Some("const")))
                    .filter(|s| !s.is_empty())
                    .into_iter()
                    .collect::<Vec<_>>()),
                _ => Value::Null,
            });
    }
    out
}
fn form_input(req: &Value) -> Option<Value> {
    let message = text(req, "message").unwrap_or("MCP server requires input");
    match text(req, "mode").unwrap_or("form") {
        "url" => {
            let uri = text(req, "url")?.parse::<Uri>().ok()?;
            if !matches!(uri.scheme_str(), Some("http" | "https")) || uri.host().is_none() {
                return None;
            }
            let url = uri.to_string();
            Some(
                json!({"url":url,"questions":[{"id":"__mcp_url_confirmation","header":"Sign in","question":message,"options":[{"label":"Open sign-in page and continue","description":url}]}]}),
            )
        }
        "form" => {
            let schema = &req["requestedSchema"];
            let mut questions = vec![];
            for (id, p) in schema["properties"].as_object()? {
                let title = text(p, "title description").unwrap_or(id);
                let mut q = json!({"id":id,"header":title,"question":format!("{message}\n\n{title}"),"required":schema["required"].as_array().is_some_and(|v|v.iter().any(|s|s==id))});
                if p["type"] == "array" {
                    q["multiple"] = json!(true);
                }
                q["options"] = json!(choices(p).iter().map(|v| json!({"label":v.as_str().map(str::to_owned).unwrap_or_else(||v.to_string()),"description":""})).collect::<Vec<_>>());
                questions.push(q);
            }
            if questions.is_empty() {
                questions.push(json!({"id":"__mcp_form_confirmation","header":"Confirmation","question":message,"options":[{"label":"Continue","description":""}]}));
            }
            Some(json!({"questions":questions}))
        }
        _ => None,
    }
}
pub(super) fn values<'a>(answers: &'a Value, id: &str) -> Vec<&'a str> {
    let v = &answers[id];
    v.as_array()
        .or_else(|| v["answers"].as_array())
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect()
}
fn coerce(v: &str, p: &Value) -> Option<Value> {
    match text(p, "type") {
        Some("boolean") => match v {
            "true" => Some(json!(true)),
            "false" => Some(json!(false)),
            _ => None,
        },
        Some("integer" | "number") => {
            let n = if v.trim().is_empty() {
                0.
            } else {
                v.trim().parse::<f64>().ok()?
            };
            if !n.is_finite() || (p["type"] == "integer" && n.fract() != 0.) {
                return None;
            }
            Some(if n.fract() == 0. && n.abs() < 9e18 {
                json!(n as i64)
            } else {
                json!(n)
            })
        }
        _ => Some(json!(v)),
    }
}
fn form_content(req: &Value, answers: &Value) -> Value {
    let mut out = json!({});
    if req["mode"] == "url" {
        return out;
    }
    for (id, p) in req["requestedSchema"]["properties"]
        .as_object()
        .into_iter()
        .flatten()
    {
        let all = values(answers, id);
        let mut selected: Vec<_> = all
            .iter()
            .copied()
            .filter(|s| !s.starts_with("user_note: "))
            .collect();
        if selected.is_empty() {
            if !choices(p).is_empty() {
                continue;
            }
            if let Some(note) = all.iter().find_map(|s| s.strip_prefix("user_note: ")) {
                selected.push(note);
            }
        }
        let Some(first) = selected.first() else {
            continue;
        };
        if p["type"] == "array" {
            out[id] = json!(selected
                .iter()
                .filter_map(|s| coerce(s, &p["items"]))
                .collect::<Vec<_>>());
        } else if let Some(v) = coerce(first, p) {
            out[id] = v;
        }
    }
    out
}
pub fn prepare(method: &str, p: &Value, shared: bool, has_user_callback: bool) -> Action {
    let fallback = || Uuid::new_v4().to_string();
    let mut id = text(p, "itemId")
        .map(str::to_owned)
        .unwrap_or_else(fallback);
    let mut input = json!({});
    let (tool, kind) = match method {
        "item/commandExecution/requestApproval" => {
            copy_fields(
                &mut input,
                p,
                &[
                    ("message", "reason"),
                    ("command", "command"),
                    ("cwd", "cwd"),
                ],
            );
            ("CodexBash", Kind::Approval)
        }
        "item/fileChange/requestApproval" => {
            copy_fields(
                &mut input,
                p,
                &[("message", "reason"), ("grantRoot", "grantRoot")],
            );
            ("CodexPatch", Kind::Approval)
        }
        "item/permissions/requestApproval" => {
            let permissions = field_or(p, "permissions", &json!({})).clone();
            copy_fields(&mut input, p, &[("message", "reason"), ("cwd", "cwd")]);
            input["permissions"] = permissions.clone();
            ("CodexPermission", Kind::Grant(permissions))
        }
        "item/tool/requestApproval" => {
            if let Some(v) = text(p, "itemId item_id") {
                id = v.into();
            }
            input = field_or(p, "input arguments", p).clone();
            (
                text(p, "toolName tool_name tool name permission").unwrap_or("CodexTool"),
                Kind::Approval,
            )
        }
        "item/tool/requestUserInput" => {
            if !has_user_callback {
                return Action::Immediate(json!({"decision":"cancel"}));
            }
            input = p.clone();
            ("request_user_input", Kind::User)
        }
        "mcpServer/elicitation/request" => {
            let req = p.get("request").filter(|r| r.is_object()).unwrap_or(p);
            id = text(req, "elicitationId")
                .map(str::to_owned)
                .unwrap_or_else(fallback);
            let meta = req
                .get("_meta")
                .filter(|v| v.is_object())
                .unwrap_or(&p["_meta"]);
            if meta["codex_approval_kind"] == "mcp_tool_call"
                && text(req, "mode").unwrap_or("form") == "form"
                && req["requestedSchema"]["properties"]
                    .as_object()
                    .is_none_or(|p| p.is_empty())
            {
                let server = text(p, "serverName").or_else(|| text(req, "serverName"));
                let tool = text(meta, "tool_name tool_title")
                    .or(server)
                    .unwrap_or("MCP tool");
                input["message"] = json!(text(req, "message").unwrap_or("Allow MCP tool call?"));
                if let Some(v) = server {
                    input["serverName"] = json!(v);
                }
                copy_fields(
                    &mut input,
                    meta,
                    &[
                        ("toolTitle", "tool_title"),
                        ("toolDescription", "tool_description"),
                    ],
                );
                for (k, v) in [
                    ("toolParams", "tool_params"),
                    ("toolParamsDisplay", "tool_params_display"),
                ] {
                    if let Some(m) = meta.get(v) {
                        input[k] = m.clone();
                    }
                }
                (
                    tool,
                    Kind::Tool(
                        meta["persist"] == "session"
                            || meta["persist"]
                                .as_array()
                                .is_some_and(|l| l.contains(&json!("session"))),
                    ),
                )
            } else if p["serverName"] == "hapi" {
                return Action::Immediate(
                    json!({"action":"accept","content":defaults(req),"_meta":null}),
                );
            } else if let Some(form) = form_input(req).filter(|_| has_user_callback) {
                input = form;
                ("request_user_input", Kind::Form(req.clone()))
            } else if shared {
                return Action::Ignore;
            } else {
                return Action::Immediate(json!({"action":"cancel","content":null,"_meta":null}));
            }
        }
        _ => return Action::Ignore,
    };
    Action::Ask(Prompt {
        id,
        tool: tool.into(),
        input,
        kind,
    })
}

#[cfg(test)]
#[test]
fn typescript_permission_vectors() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("cases");
    let out = std::process::Command::new("bun")
        .args(["test", "src/flavors/codex/permission_vectors.test.ts"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env("CODEX_PERMISSION_VECTORS", &path)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let rows: Vec<Value> = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert!(rows.len() >= 16);
    for row in rows {
        let response = match prepare(
            row["method"].as_str().unwrap(),
            &row["params"],
            row["shared"] == true,
            row["callback"] == true,
        ) {
            Action::Ignore => Value::Null,
            Action::Immediate(v) => v,
            Action::Ask(p) => {
                assert_eq!(p.tool, row["tool"]);
                assert_eq!(p.input, row["input"]);
                p.response(
                    row["answer"]["decision"].as_str().unwrap(),
                    &row["answer"]["answers"],
                )
            }
        };
        assert_eq!(response, row["response"]);
    }
}

#[cfg(test)]
#[test]
fn null_approval_payloads_use_native_fallbacks() {
    let Action::Ask(p) = prepare(
        "item/permissions/requestApproval",
        &json!({"permissions":null}),
        true,
        true,
    ) else {
        panic!()
    };
    assert_eq!(p.input["permissions"], json!({}));
    assert_eq!(
        p.response("approved", &Value::Null)["permissions"],
        json!({})
    );
    let Action::Ask(p) = prepare(
        "item/tool/requestApproval",
        &json!({"input":null,"arguments":{"path":"x"}}),
        true,
        true,
    ) else {
        panic!()
    };
    assert_eq!(p.input, json!({"path":"x"}));
}
