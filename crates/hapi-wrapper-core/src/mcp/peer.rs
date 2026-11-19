use super::tools::Options;
use crate::session::client::SessionClient;
use serde_json::Value;
use std::{io, process::Stdio};
use tokio::{io::AsyncWriteExt, process::Command};

fn agent_session(metadata: &Value) -> Option<&str> {
    let field = match metadata["flavor"].as_str() {
        Some("claude") => "claudeSessionId",
        Some("codex") => "codexSessionId",
        Some("gemini") => "geminiSessionId",
        Some("opencode") => "opencodeSessionId",
        Some("grok") => "grokSessionId",
        Some("agy") => "agySessionId",
        Some("cursor") => "cursorSessionId",
        Some("kimi") => "kimiSessionId",
        Some("copilot") => "copilotSessionId",
        Some("pi") => "piSessionId",
        Some("dsh") => "dshSessionId",
        _ => {
            return [
                "codex", "claude", "gemini", "opencode", "grok", "agy", "cursor", "kimi",
                "copilot", "dsh",
            ]
            .iter()
            .find_map(|f| metadata[format!("{f}SessionId")].as_str())
        }
    };
    metadata[field]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
}
pub(super) async fn call(
    client: &SessionClient,
    options: &Options,
    name: &str,
    args: &Value,
) -> io::Result<Value> {
    if super::tools::disabled() {
        return Err(io::Error::other(
            "Peer tools disabled by HAPI_DISABLE_PEER_TOOLS",
        ));
    }
    let metadata = client.metadata.get().value;
    let mut command =
        Command::new(std::env::var_os("HAPI_CLI_EXECUTABLE").unwrap_or_else(|| "hapi".into()));
    command
        .args(["peer-tool", name])
        .env("HAPI_SESSION_ID", &client.id);
    for (key, value) in [
        ("HAPI_SESSION_NAME", metadata["name"].as_str()),
        ("HAPI_AGENT_SESSION_ID", agent_session(&metadata)),
        (
            "HAPI_PEER_TOOL_CWD",
            options
                .working_directory
                .as_ref()
                .and_then(|p| p.to_str())
                .into_iter()
                .chain(options.skill_lookup.as_ref().and_then(|(p, _)| p.to_str()))
                .chain(metadata["path"].as_str())
                .map(str::trim)
                .find(|s| !s.is_empty()),
        ),
    ] {
        command.env_remove(key);
        if let Some(v) = value {
            command.env(key, v);
        }
    }
    command.env("HAPI_PEER_TOOL_PHASE", "call");
    let mut v = invoke(&mut command, args).await?;
    if let Some(mut watch) = v.as_object_mut().and_then(|v| v.remove("_meta")) {
        let mut stopped = client.archive_notifications();
        command.env("HAPI_PEER_TOOL_PHASE", "poll");
        tokio::spawn(async move {
            let polls = async {
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                    if super::tools::disabled() {
                        break;
                    }
                    let Ok(mut result) = invoke(&mut command, &watch).await else {
                        break;
                    };
                    let Some(next) = result.as_object_mut().and_then(|v| v.remove("_meta")) else {
                        break;
                    };
                    watch = next;
                }
            };
            tokio::select! {_=polls=>{},_=stopped.changed()=>{}}
        });
    }
    Ok(v)
}
async fn invoke(command: &mut Command, args: &Value) -> io::Result<Value> {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&serde_json::to_vec(args)?)
        .await?;
    let output = child.wait_with_output().await?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "peer-tool exited with {}",
            output.status
        )));
    }
    let v: Value = serde_json::from_slice(&output.stdout)?;
    if !v["content"].is_array() || !v["isError"].is_boolean() {
        return Err(io::Error::other("Invalid peer-tool response"));
    }
    Ok(v)
}
