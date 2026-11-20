//! Peer MCP tool delegation (`list_peers`, `ping_peer`, `inspect_peer`, `spawn_peer`)
//! via the TypeScript `hapi peer-tool <name>` CLI subcommand.
//!
//! # Known gaps
//!
//! - `hapi peer-tool` on this base handles `waitForReply` by spawning a detached
//!   `hapi peer-tool __forward_reply` helper instead of the mirror's `_meta` +
//!   `HAPI_PEER_TOOL_PHASE=poll` protocol, so closing or hub-archiving the wrapper
//!   session cancels an in-flight `peer-tool` call but does not stop an already-detached
//!   reply forwarder.
//! - Relative `spawn_peer` directories resolve against the child process's working
//!   directory (`current_dir`); if the configured session directory does not exist
//!   locally, `current_dir` falls back to the wrapper's process directory.

use super::tools::Options;
use crate::session::client::SessionClient;
use serde_json::Value;
use std::{io, path::Path, process::Stdio};
use tokio::{io::AsyncWriteExt, process::Command, sync::watch};

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
            .find_map(|f| {
                metadata[format!("{f}SessionId")]
                    .as_str()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
            });
        }
    };
    metadata[field]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn resolve_cwd<'a>(metadata: &'a Value, options: &'a Options) -> Option<&'a str> {
    options
        .working_directory
        .as_ref()
        .and_then(|p| p.to_str())
        .into_iter()
        .chain(options.skill_lookup.as_ref().and_then(|(p, _)| p.to_str()))
        .chain(metadata["path"].as_str())
        .map(str::trim)
        .find(|s| !s.is_empty())
}

pub(super) async fn call(
    client: &SessionClient,
    options: &Options,
    name: &str,
    args: &Value,
) -> io::Result<Value> {
    let command = peer_command(&client.id, &client.metadata.get().value, options, name);
    delegate(
        command,
        args,
        client.closed_notifications(),
        client.archive_notifications(),
    )
    .await
}

fn peer_command(id: &str, metadata: &Value, options: &Options, name: &str) -> Command {
    let mut command = Command::new(
        std::env::var("HAPI_CLI_EXECUTABLE")
            .ok()
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("hapi"),
    );
    command.args(["peer-tool", name]).env("HAPI_SESSION_ID", id);
    for (key, value) in [
        ("HAPI_SESSION_NAME", metadata["name"].as_str()),
        ("HAPI_AGENT_SESSION_ID", agent_session(metadata)),
    ] {
        command.env_remove(key);
        if let Some(v) = value.map(str::trim).filter(|s| !s.is_empty()) {
            command.env(key, v);
        }
    }
    if let Some(cwd) = resolve_cwd(metadata, options) {
        if Path::new(cwd).is_dir() {
            command.current_dir(cwd);
        }
    }
    command
}

async fn delegate(
    mut command: Command,
    args: &Value,
    mut closed: watch::Receiver<bool>,
    mut archived: watch::Receiver<bool>,
) -> io::Result<Value> {
    if super::tools::disabled() {
        return Err(io::Error::other(
            "Peer tools disabled by HAPI_DISABLE_PEER_TOOLS",
        ));
    }
    tokio::select! {
        biased;
        Ok(_) = closed.wait_for(|v| *v) => Err(io::Error::other("Session closed")),
        Ok(_) = archived.wait_for(|v| *v) => Err(io::Error::other("Session archived")),
        result = invoke(&mut command, args) => result,
    }
}

fn parse_tool_result(stdout: &[u8]) -> Option<Value> {
    let v: Value = serde_json::from_slice(stdout).ok()?;
    (v["content"].is_array() && v["isError"].is_boolean()).then_some(v)
}

async fn invoke(command: &mut Command, args: &Value) -> io::Result<Value> {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let mut stdin = child.stdin.take().unwrap();
    let input = serde_json::to_vec(args)?;
    let (written, output) = tokio::join!(
        async move { stdin.write_all(&input).await },
        child.wait_with_output()
    );
    let output = output?;
    if output.status.success() {
        written?;
        return parse_tool_result(&output.stdout)
            .ok_or_else(|| io::Error::other("Invalid peer-tool response"));
    }
    // `hapi peer-tool` exits 1 when returning a tool-level JSON error (`isError: true`).
    if output.status.code() == Some(1) {
        if let Some(v) = parse_tool_result(&output.stdout).filter(|v| v["isError"] == true) {
            return Ok(v);
        }
    }
    Err(io::Error::other(format!(
        "peer-tool exited with {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn agent_session_cwd_and_result_parsing() {
        assert_eq!(
            agent_session(&json!({"claudeSessionId": "legacy"})),
            Some("legacy")
        );
        assert_eq!(
            agent_session(&json!({"flavor": "codex", "codexSessionId": " native "})),
            Some("native")
        );
        assert_eq!(
            agent_session(&json!({"flavor": "codex", "claudeSessionId": "wrong"})),
            None
        );

        let meta = json!({"path": "/metadata"});
        assert_eq!(
            resolve_cwd(
                &meta,
                &Options {
                    working_directory: Some("/work".into()),
                    skill_lookup: Some(("/skill".into(), "claude".into())),
                    ..Default::default()
                }
            ),
            Some("/work")
        );
        assert_eq!(
            resolve_cwd(
                &meta,
                &Options {
                    skill_lookup: Some(("/skill".into(), "claude".into())),
                    ..Default::default()
                }
            ),
            Some("/skill")
        );
        assert_eq!(resolve_cwd(&meta, &Options::default()), Some("/metadata"));

        assert!(parse_tool_result(br#"{"content":[],"isError":false}"#).is_some());
        assert!(parse_tool_result(br#"{"content":[],"isError":true}"#).is_some());
        assert!(parse_tool_result(b"invalid").is_none());
        assert!(parse_tool_result(br#"{"content":[]}"#).is_none());
    }
}
