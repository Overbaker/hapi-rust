use super::http::{self, Tools};
pub use crate::loopback::Listener;
use crate::{rpc::handlers::resolve_skill, session::client::SessionClient};
use futures_util::future::BoxFuture;
use serde_json::{json, Value};
use std::{io, path::PathBuf, sync::Arc, time::Duration};

pub struct Options {
    pub emit_title_summary: bool,
    pub enable_change_title: bool,
    pub on_change_title: Option<Arc<dyn Fn(String) + Send + Sync>>,
    pub working_directory: Option<PathBuf>,
    pub skill_lookup: Option<(PathBuf, String)>,
    pub enabled_tools: Option<Vec<String>>,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            emit_title_summary: true,
            enable_change_title: true,
            on_change_title: None,
            working_directory: None,
            skill_lookup: None,
            enabled_tools: None,
        }
    }
}
struct HapiTools {
    client: Arc<SessionClient>,
    options: Options,
    catalog: Value,
}
pub(super) fn disabled() -> bool {
    matches!(
        std::env::var("HAPI_DISABLE_PEER_TOOLS")
            .unwrap_or_default()
            .to_lowercase()
            .as_str(),
        "1" | "true" | "yes"
    )
}
pub(super) fn catalog(options: &Options, nopeer: bool) -> Value {
    let mut catalog: Value = serde_json::from_str(if nopeer {
        include_str!("../../golden/mcp_tools_nopeer.json")
    } else {
        include_str!("../../golden/mcp_tools_all.json")
    })
    .unwrap();
    catalog["tools"].as_array_mut().unwrap().retain(|tool| {
        let name = tool["name"].as_str().unwrap();
        (name != "skill_lookup" || options.skill_lookup.is_some())
            && (name != "change_title" || options.enable_change_title)
            && options
                .enabled_tools
                .as_ref()
                .is_none_or(|names| names.iter().any(|s| s == name))
    });
    catalog
}
pub async fn start(client: Arc<SessionClient>, options: Options) -> io::Result<Listener> {
    let catalog = catalog(&options, disabled());
    if catalog["tools"].as_array().unwrap().is_empty() {
        return Err(io::Error::other(
            "startHappyServer requires at least one enabled tool",
        ));
    }
    let server = http::serve(Arc::new(HapiTools {
        client: client.clone(),
        options,
        catalog,
    }))
    .await?;
    let url = format!("http://127.0.0.1:{}/", server.port);
    client.metadata.update(move |m| {
        let mut m = m.clone();
        m["hapiMcpUrl"] = json!(url);
        m
    });
    Ok(server)
}
fn fits(v: &Value, s: &Value) -> bool {
    if let Some(items) = s["anyOf"].as_array() {
        return items.iter().any(|s| fits(v, s));
    }
    if let Some(values) = s["enum"].as_array() {
        return values.contains(v);
    }
    match s["type"].as_str() {
        Some("integer" | "number") => v.as_f64().is_some_and(|n| {
            (s["type"] != "integer" || n.fract() == 0.)
                && s["minimum"].as_f64().is_none_or(|m| n >= m)
                && s["maximum"].as_f64().is_none_or(|m| n <= m)
                && s["exclusiveMinimum"].as_f64().is_none_or(|m| n > m)
        }),
        Some("string") => v.as_str().is_some_and(|v| {
            let len = v.encode_utf16().count() as u64;
            len >= s["minLength"].as_u64().unwrap_or(0)
                && len <= s["maxLength"].as_u64().unwrap_or(u64::MAX)
        }),
        Some("boolean") => v.is_boolean(),
        Some("null") => v.is_null(),
        _ => false,
    }
}
pub(super) fn validate(tool: &Value, args: Value) -> Result<Value, String> {
    let schema = &tool["inputSchema"];
    let props = schema["properties"].as_object().unwrap();
    let mut args = args.as_object().cloned().ok_or("Expected object")?;
    args.retain(|k, _| props.contains_key(k));
    for (k, s) in props {
        if let Some(v) = args.get_mut(k) {
            let trim = matches!(
                k.as_str(),
                "sessionIdPrefix"
                    | "directory"
                    | "machine"
                    | "model"
                    | "effort"
                    | "name"
                    | "unit"
                    | "label"
                    | "jobKey"
            ) || (tool["name"] == "display_media" && k == "title");
            if trim {
                if let Some(raw) = v.as_str() {
                    *v = json!(raw.trim())
                }
            }
            if !fits(v, s) {
                return Err(format!("Invalid argument: {k}"));
            }
        } else if schema["required"]
            .as_array()
            .is_some_and(|arr| arr.iter().any(|n| n == k))
        {
            return Err(format!("Missing argument: {k}"));
        }
    }
    Ok(Value::Object(args))
}
pub(super) fn result(r: Result<String, String>) -> Value {
    let error = r.is_err();
    json!({"content":[{"type":"text","text":r.unwrap_or_else(|e|e)}],"isError":error})
}
impl Tools for HapiTools {
    fn list(&self) -> Value {
        self.catalog.clone()
    }
    fn call(&self, name: String, args: Value) -> BoxFuture<'_, Value> {
        Box::pin(async move {
            let Some(tool) = self.catalog["tools"]
                .as_array()
                .unwrap()
                .iter()
                .find(|t| t["name"] == name)
            else {
                return result(Err(format!("Unknown tool {name}")));
            };
            let args = match validate(tool, args) {
                Ok(v) => v,
                Err(e) => return result(Err(e)),
            };
            let client = self.client.clone();
            let response = match name.as_str() {
                "list_peers" | "ping_peer" | "inspect_peer" | "spawn_peer" => {
                    return super::peer::call(&client, &self.options, &name, &args)
                        .await
                        .unwrap_or_else(|e| result(Err(e.to_string())))
                }
                "session_job" => super::job::call(&client.rest, &client.id, &args).await,
                "change_title" => self
                    .title(args["title"].as_str().unwrap())
                    .await
                    .map_err(|e| format!("Failed to change chat title: {e}")),
                "skill_lookup" => {
                    let (cwd, flavor) = self.options.skill_lookup.clone().unwrap();
                    tokio::task::spawn_blocking(move || {
                        resolve_skill(&flavor, &cwd, args["name"].as_str().unwrap())
                    })
                    .await
                    .map_err(|e| e.to_string())
                    .and_then(|r| r)
                    .map_err(|e| format!("Failed to look up skill: {e}"))
                }
                "display_media" | "display_image" | "display_video" => {
                    let kind = name[8..].to_owned();
                    tokio::task::spawn_blocking(move || {
                        super::media::display(&client, &name, &args)
                    })
                    .await
                    .map_err(|e| e.to_string())
                    .and_then(|r| r)
                    .map_err(|e| format!("Failed to display {kind}: {e}"))
                }
                _ => Err(format!("Unknown tool {name}")),
            };
            result(response)
        })
    }
}
impl HapiTools {
    async fn title(&self, raw: &str) -> Result<String, String> {
        const LOCKED: &str = "Session title is locked by the user. Only the user can unlock it.";
        let title = raw.split_whitespace().collect::<Vec<_>>().join(" ");
        if title.is_empty() {
            return Err("Title must not be empty".into());
        }
        let client = &self.client;
        if !client.refresh_metadata().await {
            return Err("Could not read the current title lock from the Hub.".into());
        }
        let check = || {
            if client.metadata.get().value["titleLocked"] == true {
                Err(LOCKED.to_owned())
            } else {
                Ok(())
            }
        };
        let flush = || async {
            if !client.flush_metadata(Duration::from_secs(5)).await {
                Err("Title update could not be confirmed by the Hub.".into())
            } else {
                check()
            }
        };
        check()?;
        if self.options.emit_title_summary {
            let name = title.clone();
            client.metadata.update(move |m| {
                let mut m = m.clone();
                if m["titleLocked"] != true && m["name"].as_str().map(str::trim) != Some(&name) {
                    m["name"] = json!(name)
                }
                m
            });
            flush().await?;
            if let Some(callback) = &self.options.on_change_title {
                callback(title.clone());
                flush().await?;
            }
        }
        Ok(format!("Successfully changed chat title to: \"{title}\""))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tool_filters_and_validation() {
        let options = Options {
            skill_lookup: Some(("/tmp".into(), "codex".into())),
            ..Default::default()
        };
        for (nopeer, expected) in [
            (true, include_str!("../../golden/mcp_tools_nopeer.json")),
            (false, include_str!("../../golden/mcp_tools_all.json")),
        ] {
            assert_eq!(
                catalog(&options, nopeer),
                serde_json::from_str::<Value>(expected).unwrap()
            );
        }
        let c = catalog(&options, false);
        let tool = c["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "session_job")
            .unwrap();
        assert_eq!(
            validate(
                tool,
                json!({"action":"update","jobKey":" x ","remaining":null,"sessionId":"other"})
            )
            .unwrap(),
            json!({"action":"update","jobKey":"x","remaining":null})
        );
        assert!(validate(tool, json!({"action":"update","total":0})).is_err());
        let limited = Options {
            enabled_tools: Some(vec![
                "ping_peer".into(),
                "skill_lookup".into(),
                "display_image".into(),
            ]),
            ..Default::default()
        };
        assert_eq!(
            catalog(&limited, true)["tools"].as_array().unwrap().len(),
            1
        );
    }
}
