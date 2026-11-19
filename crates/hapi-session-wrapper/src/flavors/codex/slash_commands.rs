use super::value::*;
use serde_json::{json, Value};
fn handled(message: impl AsRef<str>) -> Value {
    json!({"kind":"handled","message":message.as_ref()})
}
fn update(message: impl AsRef<str>, updates: Value) -> Value {
    merge(handled(message), &json!({"updates":updates}))
}
pub fn resolve(input: &str, state: &Value) -> Value {
    let pass = json!({"kind":"passthrough"});
    let Some(body) = input.trim_start().strip_prefix('/') else {
        return pass;
    };
    let (command, rest) = body.split_once(char::is_whitespace).unwrap_or((body, ""));
    if command.is_empty()
        || !command
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b":_-".contains(&b))
    {
        return pass;
    }
    let command = command.to_ascii_lowercase();
    let rest = rest.trim();
    let lower = rest.to_lowercase();
    if let Some(c) = state["commands"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|c| {
            c["source"] != "builtin" && text(c, "name").is_some_and(|n| n.to_lowercase() == command)
        })
    {
        if let Some(s) = text(c, "content") {
            return json!({"kind":"replace","text":if rest.is_empty(){s.into()}else{format!("{s}\n\nUser arguments: {rest}")},"message":format!("Expanded /{}",c["name"].as_str().unwrap())});
        }
    }
    let val = |k: &str, default: &str| state[k].as_str().unwrap_or(default).to_owned();
    match command.as_str() {
        "plan" => {
            if matches!(lower.as_str(), "off" | "default" | "exit" | "disable") {
                return update("Codex plan mode disabled", json!({"collaborationMode":"default"}));
            }
            let mut r = update("Codex plan mode enabled", json!({"collaborationMode":"plan"}));
            if !rest.is_empty() {
                r["kind"] = json!("replace");
                r["text"] = json!(rest);
            }
            r
        }
        "agent" => {
            let active = state["proactiveMultiAgent"] == true;
            let enabled = match lower.as_str() {
                "status" => {
                    return handled(format!(
                        "Codex proactive multi-agent mode: {}",
                        if active {
                            "on"
                        } else {
                            "off"
                        }
                    ))
                }
                "" => !active,
                "on" | "enable" | "enabled" => true,
                "off" | "disable" | "disabled" => false,
                _ => return handled("Usage: /agent [on|off|status]"),
            };
            update(
                format!(
                    "Codex proactive multi-agent mode {}",
                    if enabled {
                        "enabled"
                    } else {
                        "disabled"
                    }
                ),
                json!({"proactiveMultiAgent":enabled}),
            )
        }
        "goal" => match lower.as_str() {
            "" => json!({"kind":"goal","action":"show"}),
            "clear" | "pause" | "resume" => json!({"kind":"goal","action":lower}),
            _ if rest.chars().count() > 4000 => handled("Goal objective must be at most 4000 characters."),
            _ => json!({"kind":"goal","action":"set","objective":rest}),
        },
        "default" | "execute" => update("Codex collaboration mode: default", json!({"collaborationMode":"default"})),
        "status" => handled(format!("**Codex status**\n\n- permission: `{}`\n- collaboration: `{}`\n- model: `{}`\n- reasoning: `{}`\n- personality: `{}`", val("permissionMode", "undefined"), val("collaborationMode", "undefined"), val("model", "auto"), val("modelReasoningEffort", "default"), val("personality", "unset"))),
        "personality" => match rest {
            "" => handled(format!("Codex personality: {}", val("personality", "unset (Codex config / thread sticky)"))),
            "default" | "auto" | "clear" => handled("Codex personality is sticky on the thread; set friendly, pragmatic, or none (cannot restore config.toml by clearing)"),
            "friendly" | "pragmatic" | "none" => update(format!("Codex personality set to {rest}"), json!({"personality":rest})),
            _ => handled(format!("Unknown Codex personality: {rest}")),
        },
        "model" => {
            if rest.is_empty() {
                handled(format!("Codex model: {}", val("model", "auto")))
            } else {
                update(format!("Codex model set to {rest}"), json!({"model":(rest!="auto").then_some(rest)}))
            }
        }
        "reasoning" | "effort" => match rest {
            "" => handled(format!("Codex reasoning effort: {}", val("modelReasoningEffort", "default"))),
            "default" | "auto" => update("Codex reasoning effort set to default", json!({"modelReasoningEffort":null})),
            _ => update(format!("Codex reasoning effort set to {lower}"), json!({"modelReasoningEffort":lower})),
        },
        "fast" => match lower.as_str() {
            "" | "on" => update("Codex Fast mode enabled", json!({"serviceTier":"fast"})),
            "off" => update("Codex Fast mode disabled", json!({"serviceTier":"standard"})),
            "status" => handled(format!(
                "Codex Fast mode: {}",
                if state["serviceTier"] == "fast" {
                    "on"
                } else {
                    "off"
                }
            )),
            _ => handled("Usage: /fast [on|off|status]"),
        },
        "permissions" | "permission" => match rest {
            "" => handled(format!("Codex permission mode: {}", val("permissionMode", "undefined"))),
            "default" | "read-only" | "autoReview" | "yolo" => update(format!("Codex permission mode set to {rest}"), json!({"permissionMode":rest})),
            _ => handled(format!("Unknown Codex permission mode: {rest}")),
        },
        "help" => handled(HELP),
        "compat" | "diff" | "init" | "login" | "logout" | "mcp" | "new" | "prompts" | "quit" | "redo" | "review" | "undo" => handled(format!("/{command} is a Codex CLI command that is not supported in HAPI sessions yet.")),
        _ => pass,
    }
}

const HELP: &str = "**Supported Codex slash commands**\n\n- `/plan [prompt]` — enable plan mode, optionally send prompt\n- `/plan off` — return to default mode\n- `/goal [objective]` — set or view the persistent goal\n- `/goal pause|resume|clear` — update the current goal\n- `/clear` — reset current Codex thread context\n- `/compact` — compact current Codex thread context\n- `/status` — show current Codex session config\n- `/model [name|auto]` — show or set model\n- `/reasoning [level|default]` — show or set reasoning effort\n- `/personality [friendly|pragmatic|none]` — show or set response style (sticky on thread)\n- `/fast [on|off|status]` — toggle Fast mode (GPT-5.5 / GPT-5.4, ChatGPT login)\n- `/permissions [default|read-only|autoReview|yolo]` — show or set permission mode\n\nCustom `/commands` from `.codex/prompts` are expanded before sending.";
