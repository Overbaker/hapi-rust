use super::tools::Options;
use crate::session::client::SessionClient;
use serde_json::Value;
use std::{io, process::Stdio, time::Duration};
use tokio::sync::watch;
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
    let command = peer_command(&client.id, &client.metadata.get().value, options, name);
    delegate(
        command,
        args,
        client.closed_notifications(),
        client.archive_notifications(),
        Duration::from_secs(10),
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
    command
}

async fn delegate(
    mut command: Command,
    args: &Value,
    mut closed: watch::Receiver<bool>,
    mut archived: watch::Receiver<bool>,
    interval: Duration,
) -> io::Result<Value> {
    if super::tools::disabled() {
        return Err(io::Error::other(
            "Peer tools disabled by HAPI_DISABLE_PEER_TOOLS",
        ));
    }
    let mut v = invoke(&mut command, args).await?;
    if let Some(state) = v.as_object_mut().and_then(|v| v.remove("_meta")) {
        let deadline = state["state"]["deadline"]
            .as_u64()
            .ok_or_else(|| io::Error::other("Invalid peer reply deadline"))?;
        let remaining =
            Duration::from_millis(deadline.saturating_sub(crate::session::metadata::now()));
        command.env("HAPI_PEER_TOOL_PHASE", "poll");
        tokio::spawn(async move {
            tokio::select! {
                biased;
                _ = closed.wait_for(|v| *v) => {}
                _ = archived.wait_for(|v| *v) => {}
                _ = tokio::time::sleep(remaining) => eprintln!("peer reply polling deadline expired"),
                result = poll_reply(&mut command,state,interval) => {
                    if let Err(e) = result { eprintln!("peer reply polling stopped: {e}"); }
                }
            }
        });
    }
    Ok(v)
}
async fn poll_reply(command: &mut Command, mut state: Value, interval: Duration) -> io::Result<()> {
    let mut failures = 0;
    loop {
        tokio::time::sleep(interval).await;
        if super::tools::disabled() {
            return Ok(());
        }
        match invoke(command, &state).await.and_then(|v| {
            if v["isError"] == true {
                Err(io::Error::other(
                    v["content"][0]["text"]
                        .as_str()
                        .unwrap_or("peer poll failed")
                        .to_owned(),
                ))
            } else {
                Ok(v)
            }
        }) {
            Ok(mut v) => {
                let Some(next) = v.as_object_mut().and_then(|r| r.remove("_meta")) else {
                    return Ok(());
                };
                state = next;
                failures = 0;
            }
            Err(e) => {
                failures += 1;
                if failures >= 5 {
                    return Err(e);
                }
            }
        }
    }
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
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "peer-tool exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    written?;
    let v: Value = serde_json::from_slice(&output.stdout)?;
    if !v["content"].is_array() || !v["isError"].is_boolean() {
        return Err(io::Error::other("Invalid peer-tool response"));
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::{fs, os::unix::fs::PermissionsExt, path::Path};

    async fn until(condition: impl Fn() -> bool) {
        tokio::time::timeout(Duration::from_secs(3), async {
            while !condition() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
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
                .env("HAPI_PEER_TOOL_CWD", "stale")
                .status()
                .unwrap();
            assert!(result.success());
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let exe = root.join("peer helper");
        fs::write(&exe,r#"#!/bin/sh
set -eu
phase="$HAPI_PEER_TOOL_PHASE"
cat > "$phase.stdin"
printf '%s\n' "$@" > argv
printf '%s\n' "$HAPI_SESSION_ID" "${HAPI_SESSION_NAME-unset}" "${HAPI_AGENT_SESSION_ID-unset}" "${HAPI_PEER_TOOL_CWD-unset}" > identity
if [ "$phase" = poll ]; then echo poll >> attempts; fi
case "$MODE" in
    fail) echo 'stub failure' >&2; exit 23;;
    malformed) echo 'invalid'; exit 0;;
esac
if [ "$phase" = call ]; then
    printf '{"content":[],"isError":false,"_meta":{"state":{"deadline":%s}}}\n' "$DEADLINE"
elif [ "$MODE" = block ]; then
    echo "$$" > pid
    exec sleep 30
elif [ ! -f recovered ]; then
    touch recovered
    exit 1
else
    echo '{"content":[],"isError":false}'
fi
"#).unwrap();
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
        let future = crate::session::metadata::now() + 60_000;
        let command = |m: &Value, opts: &Options, mode: &str, deadline: u64| {
            let mut c = peer_command("caller", m, opts, "ping_peer");
            c.current_dir(root)
                .env("MODE", mode)
                .env("DEADLINE", deadline.to_string());
            c
        };
        let (closed, _) = watch::channel(false);
        let (archived, _) = watch::channel(false);
        let args = json!({"message":"hello world"});
        let interval = Duration::from_millis(8);
        let response = delegate(
            command(&empty, &opts, "ok", future),
            &args,
            closed.subscribe(),
            archived.subscribe(),
            interval,
        )
        .await
        .unwrap();
        assert_eq!(response, json!({"content":[],"isError":false}));
        until(|| {
            fs::read_to_string(root.join("attempts"))
                .unwrap_or_default()
                .lines()
                .count()
                == 2
        })
        .await;
        assert_eq!(
            fs::read_to_string(root.join("argv")).unwrap(),
            "peer-tool\nping_peer\n"
        );
        assert_eq!(
            fs::read_to_string(root.join("identity")).unwrap(),
            "caller\nunset\nunset\nunset\n"
        );
        for (file, expected) in [
            ("call.stdin", args.clone()),
            ("poll.stdin", json!({"state":{"deadline":future}})),
        ] {
            assert_eq!(
                serde_json::from_slice::<Value>(&fs::read(root.join(file)).unwrap()).unwrap(),
                expected
            );
        }
        let meta = json!({"flavor":"codex","codexSessionId":" native ","name":"Parent","path":"/metadata"});
        for (working, skill, expected) in [
            (Some("/work"), Some("/skill"), "/work"),
            (None, Some("/skill"), "/skill"),
            (None, None, "/metadata"),
        ] {
            let options = Options {
                working_directory: working.map(Into::into),
                skill_lookup: skill.map(|s| (s.into(), "claude".into())),
                ..Default::default()
            };
            invoke(&mut command(&meta, &options, "ok", future), &args)
                .await
                .unwrap();
            assert_eq!(
                fs::read_to_string(root.join("identity")).unwrap(),
                format!("caller\nParent\nnative\n{expected}\n")
            );
        }
        assert_eq!(
            agent_session(&json!({"claudeSessionId":"legacy"})),
            Some("legacy")
        );
        assert_eq!(
            agent_session(&json!({"flavor":"codex","claudeSessionId":"wrong"})),
            None
        );
        assert!(
            invoke(&mut command(&empty, &opts, "malformed", future), &args)
                .await
                .is_err()
        );
        let mut fail = command(&empty, &opts, "fail", future);
        fail.env("HAPI_PEER_TOOL_PHASE", "poll");
        fs::remove_file(root.join("attempts")).unwrap();
        let err = poll_reply(&mut fail, args.clone(), interval)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("stub failure"));
        assert_eq!(
            fs::read_to_string(root.join("attempts"))
                .unwrap()
                .lines()
                .count(),
            5
        );

        for close in [false, true] {
            let (closed, _) = watch::channel(false);
            let (archive, _) = watch::channel(false);
            delegate(
                command(&empty, &opts, "block", future),
                &args,
                closed.subscribe(),
                archive.subscribe(),
                interval,
            )
            .await
            .unwrap();
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
            until(|| !Path::new(&proc).exists()).await;
            fs::remove_file(pid_file).unwrap();
        }
        fs::remove_file(root.join("attempts")).unwrap();
        for (closed, archived, deadline) in [
            (false, true, future),
            (true, false, future),
            (false, false, 0),
        ] {
            let (closed, _) = watch::channel(closed);
            let (archive, _) = watch::channel(archived);
            delegate(
                command(&empty, &opts, "ok", deadline),
                &args,
                closed.subscribe(),
                archive.subscribe(),
                interval,
            )
            .await
            .unwrap();
            tokio::time::sleep(interval * 3).await;
            assert!(!root.join("attempts").exists());
        }
        fs::remove_file(root.join("call.stdin")).unwrap();
        std::env::set_var("HAPI_DISABLE_PEER_TOOLS", "YES");
        assert!(delegate(
            command(&empty, &opts, "ok", future),
            &args,
            closed.subscribe(),
            archived.subscribe(),
            interval
        )
        .await
        .is_err());
        assert!(!root.join("call.stdin").exists());
    }
}
