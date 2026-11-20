//! Peer MCP tool delegation (`list_peers`, `ping_peer`, `inspect_peer`, `spawn_peer`)
//! via the TypeScript `hapi peer-tool <name>` CLI subcommand.
//!
//! # Known gaps
//!
//! - `waitForReply` forwarding runs as a `hapi peer-tool __forward_reply` child
//!   of the wrapper (requested with `HAPI_PEER_TOOL_FORWARD_REPLY=caller`) so the
//!   heartbeat can report `hasPeerReplyWatch`; it is killed when the session closes.
//!   An older `hapi` without that switch still detaches the forwarder, and then
//!   the watch is not reported.
//! - Relative `spawn_peer` directories resolve against the child process's working
//!   directory (`current_dir`); if the configured session directory does not exist
//!   locally, `current_dir` falls back to the wrapper's process directory.

use super::tools::Options;
use crate::session::client::SessionClient;
use serde_json::Value;
use std::{io, path::Path, process::Stdio, sync::Arc};
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
    client: &Arc<SessionClient>,
    options: &Options,
    name: &str,
    args: &Value,
) -> io::Result<Value> {
    let metadata = client.metadata.get().value;
    let mut command = peer_command(&client.id, &metadata, options, name);
    command.env(FORWARD_REPLY_ENV, "caller");
    let mut result = delegate(
        command,
        args,
        client.closed_notifications(),
        client.archive_notifications(),
    )
    .await?;
    if let Some(spec) = take_forward_reply(&mut result) {
        let mut forwarder = peer_command(&client.id, &metadata, options, "__forward_reply");
        forwarder.args(spec);
        let client = client.clone();
        tokio::spawn(async move {
            let closed = client.closed_notifications();
            client
                .track_peer_reply_watch(forward_reply(forwarder, closed))
                .await;
        });
    }
    Ok(result)
}

/// Asks `hapi peer-tool` to hand `waitForReply` forwarding back to this wrapper
/// so the pending watch shows up as `hasPeerReplyWatch` in the heartbeat.
const FORWARD_REPLY_ENV: &str = "HAPI_PEER_TOOL_FORWARD_REPLY";

fn take_forward_reply(result: &mut Value) -> Option<[String; 3]> {
    let meta = result.as_object_mut()?.remove("_meta")?;
    let spec = &meta["hapiForwardReply"];
    let field = |k: &str| {
        spec[k]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    Some([
        field("peerSessionId")?,
        field("taskLocalId")?,
        field("callerSessionId")?,
    ])
}

async fn forward_reply(mut command: Command, mut closed: watch::Receiver<bool>) {
    let child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn();
    let Ok(mut child) = child else { return };
    tokio::select! {
        _ = closed.wait_for(|v| *v) => {}
        _ = child.wait() => {}
    }
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use serde_json::json;
    use std::{fs, os::unix::fs::PermissionsExt, time::Duration};

    async fn until(condition: impl Fn() -> bool) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while !condition() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
    }

    #[test]
    fn forward_reply_spec_is_taken_from_the_result() {
        let mut result = json!({
            "content":[{"type":"text","text":"ok"}],
            "isError":false,
            "_meta":{"hapiForwardReply":{"peerSessionId":"p","taskLocalId":"t","callerSessionId":"c"}}
        });
        assert_eq!(
            take_forward_reply(&mut result),
            Some(["p".into(), "t".into(), "c".into()])
        );
        assert_eq!(
            result,
            json!({"content":[{"type":"text","text":"ok"}],"isError":false})
        );
        let mut plain = json!({"content":[],"isError":false});
        assert_eq!(take_forward_reply(&mut plain), None);
        let mut partial = json!({"content":[],"isError":false,"_meta":{"hapiForwardReply":{"peerSessionId":"p"}}});
        assert_eq!(take_forward_reply(&mut partial), None);
    }

    #[tokio::test]
    async fn subprocess_contract() {
        if std::env::var_os("HAPI_PEER_CONTRACT_CHILD").is_none() {
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "mcp::peer::tests::subprocess_contract"])
                .env("HAPI_PEER_CONTRACT_CHILD", "1")
                .env_remove("HAPI_DISABLE_PEER_TOOLS")
                .env("HAPI_SESSION_NAME", "stale")
                .env("HAPI_AGENT_SESSION_ID", "stale")
                .status()
                .unwrap();
            assert!(result.success());
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let work = root.join("work");
        let skill = root.join("skill");
        let meta_dir = root.join("metadata");
        for dir in [&work, &skill, &meta_dir] {
            fs::create_dir(dir).unwrap();
        }
        let exe = root.join("peer helper");
        fs::write(
            &exe,
            r#"#!/bin/sh
set -eu
cat > "$OUT/call.stdin"
printf '%s\n' "$@" > "$OUT/argv"
printf '%s\n' "$HAPI_SESSION_ID" "${HAPI_SESSION_NAME-unset}" "${HAPI_AGENT_SESSION_ID-unset}" "$(pwd)" > "$OUT/identity"
case "$MODE" in
    tool_error)
        printf '{"content":[{"type":"text","text":"Failed to inspect peer: not found"}],"isError":true}\n'
        exit 1
        ;;
    crash)
        echo 'stub crash' >&2
        exit 1
        ;;
    usage)
        echo 'hapi peer-tool: unknown peer tool' >&2
        exit 2
        ;;
    malformed)
        echo 'invalid'
        exit 0
        ;;
    block)
        echo "$$" > "$OUT/pid"
        exec sleep 30
        ;;
    *)
        printf '{"content":[{"type":"text","text":"ok"}],"isError":false}\n'
        ;;
esac
"#,
        )
        .unwrap();
        fs::set_permissions(&exe, fs::Permissions::from_mode(0o700)).unwrap();

        std::env::set_var("HAPI_CLI_EXECUTABLE", "  ");
        let empty = json!({});
        let opts = Options::default();
        assert_eq!(
            peer_command("id", &empty, &opts, "list_peers")
                .as_std()
                .get_program(),
            "hapi"
        );

        std::env::set_var("HAPI_CLI_EXECUTABLE", &exe);
        let command = |m: &Value, opts: &Options, mode: &str| {
            let mut c = peer_command("caller", m, opts, "ping_peer");
            c.env("OUT", root).env("MODE", mode);
            c
        };
        let (closed, _) = watch::channel(false);
        let (archived, _) = watch::channel(false);
        let args = json!({"message":"hello world"});

        let response = delegate(
            command(&empty, &opts, "ok"),
            &args,
            closed.subscribe(),
            archived.subscribe(),
        )
        .await
        .unwrap();
        assert_eq!(
            response,
            json!({"content":[{"type":"text","text":"ok"}],"isError":false})
        );
        assert_eq!(
            fs::read_to_string(root.join("argv")).unwrap(),
            "peer-tool\nping_peer\n"
        );
        let identity = fs::read_to_string(root.join("identity")).unwrap();
        let mut lines = identity.lines();
        assert_eq!(lines.next(), Some("caller"));
        assert_eq!(lines.next(), Some("unset"));
        assert_eq!(lines.next(), Some("unset"));
        assert_eq!(
            serde_json::from_slice::<Value>(&fs::read(root.join("call.stdin")).unwrap()).unwrap(),
            args
        );

        // Exit code 1 with valid MCP JSON `isError: true` preserves the tool result.
        let tool_err = delegate(
            command(&empty, &opts, "tool_error"),
            &args,
            closed.subscribe(),
            archived.subscribe(),
        )
        .await
        .unwrap();
        assert_eq!(
            tool_err,
            json!({"content":[{"type":"text","text":"Failed to inspect peer: not found"}],"isError":true})
        );

        // Exit code 1 crash (non-JSON stdout), exit code 2 usage error, and malformed exit 0 fail.
        let crash = invoke(&mut command(&empty, &opts, "crash"), &args)
            .await
            .unwrap_err();
        assert!(crash.to_string().contains("stub crash"));
        let usage = invoke(&mut command(&empty, &opts, "usage"), &args)
            .await
            .unwrap_err();
        assert!(usage.to_string().contains("unknown peer tool"));
        assert!(invoke(&mut command(&empty, &opts, "malformed"), &args)
            .await
            .is_err());

        // Cwd precedence: working_directory > skill_lookup > metadata.path, plus identity env vars.
        let meta = json!({
            "flavor": "codex",
            "codexSessionId": " native ",
            "name": "Parent",
            "path": meta_dir
        });
        for (working, skill_dir, expected) in [
            (Some(work.as_path()), Some(skill.as_path()), work.as_path()),
            (None, Some(skill.as_path()), skill.as_path()),
            (None, None, meta_dir.as_path()),
        ] {
            let options = Options {
                working_directory: working.map(Into::into),
                skill_lookup: skill_dir.map(|s| (s.into(), "claude".into())),
                ..Default::default()
            };
            invoke(&mut command(&meta, &options, "ok"), &args)
                .await
                .unwrap();
            let expected = fs::canonicalize(expected).unwrap();
            assert_eq!(
                fs::read_to_string(root.join("identity")).unwrap(),
                format!("caller\nParent\nnative\n{}\n", expected.display())
            );
        }

        // Non-existent working_directory does not prevent spawning the helper.
        let missing = Options {
            working_directory: Some(root.join("does-not-exist")),
            ..Default::default()
        };
        assert!(invoke(&mut command(&empty, &missing, "ok"), &args)
            .await
            .is_ok());

        assert_eq!(
            agent_session(&json!({"claudeSessionId":"legacy"})),
            Some("legacy")
        );
        assert_eq!(
            agent_session(&json!({"flavor":"codex","claudeSessionId":"wrong"})),
            None
        );

        // Closing or archiving cancels an in-flight peer-tool subprocess and kills it.
        for close in [false, true] {
            let (closed, _) = watch::channel(false);
            let (archive, _) = watch::channel(false);
            let call_args = args.clone();
            let call_cmd = command(&empty, &opts, "block");
            let closed_rx = closed.subscribe();
            let archive_rx = archive.subscribe();
            let call =
                tokio::spawn(
                    async move { delegate(call_cmd, &call_args, closed_rx, archive_rx).await },
                );
            let pid_file = root.join("pid");
            until(|| fs::read_to_string(&pid_file).is_ok_and(|s| s.trim().parse::<u32>().is_ok()))
                .await;
            let pid = fs::read_to_string(&pid_file).unwrap();
            let proc = format!("/proc/{}", pid.trim());
            if close {
                closed.send_replace(true);
            } else {
                archive.send_replace(true);
            }
            assert!(call.await.unwrap().is_err());
            until(|| !Path::new(&proc).exists()).await;
            fs::remove_file(pid_file).unwrap();
        }

        // Pre-closed / pre-archived sessions fail immediately without spawning.
        for (is_closed, is_archived) in [(true, false), (false, true)] {
            fs::remove_file(root.join("call.stdin")).ok();
            let (closed, _) = watch::channel(is_closed);
            let (archive, _) = watch::channel(is_archived);
            assert!(delegate(
                command(&empty, &opts, "ok"),
                &args,
                closed.subscribe(),
                archive.subscribe(),
            )
            .await
            .is_err());
            assert!(!root.join("call.stdin").exists());
        }

        // HAPI_DISABLE_PEER_TOOLS refuses without spawning.
        fs::remove_file(root.join("call.stdin")).ok();
        std::env::set_var("HAPI_DISABLE_PEER_TOOLS", "YES");
        assert!(delegate(
            command(&empty, &opts, "ok"),
            &args,
            closed.subscribe(),
            archived.subscribe(),
        )
        .await
        .is_err());
        assert!(!root.join("call.stdin").exists());
    }
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
