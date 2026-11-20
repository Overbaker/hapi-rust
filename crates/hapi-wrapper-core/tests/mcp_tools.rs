use bytes::Bytes;
use hapi_wrapper_core::{
    config::Config,
    mcp::{
        http, stdio_bridge,
        tools::{self, Options},
    },
    rest::Rest,
    rpc::handlers::{files, GeneratedImages},
    session::client::{SessionClient, SessionEvent},
};
use http_body_util::{BodyExt, Full};
use hyper::Request;
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
use serde_json::{json, Value};
use std::{
    fs,
    path::Path,
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines},
    process::{Child, ChildStdout, Command},
    sync::mpsc,
    time::timeout,
};

const LIMIT: Duration = Duration::from_secs(15);

struct Hub {
    child: Child,
    lines: Lines<BufReader<ChildStdout>>,
    config: Config,
    _dir: tempfile::TempDir,
}

impl Hub {
    async fn new() -> Self {
        let mut child = Command::new("bun")
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/session_hub.cjs"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        let port = timeout(LIMIT, lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .parse::<u16>()
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            home: dir.path().into(),
            api_url: format!("http://127.0.0.1:{port}"),
            cli_api_token: "token".into(),
            extra_headers: vec![("x-fixture".into(), "extra".into())],
            machine_id: Some("m".into()),
        };
        Self {
            child,
            lines,
            config,
            _dir: dir,
        }
    }

    async fn command(&mut self, op: &str, value: Value) -> Value {
        let line = format!("{}\n", json!({"op": op, "value": value}));
        self.child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(line.as_bytes())
            .await
            .unwrap();
        let answer = timeout(LIMIT, self.lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        serde_json::from_str(&answer).unwrap()
    }

    async fn start_with_metadata(
        &self,
        metadata: Value,
    ) -> (Arc<SessionClient>, mpsc::UnboundedReceiver<SessionEvent>) {
        SessionClient::start(
            &self.config,
            Rest::new(&self.config).await.unwrap(),
            &json!({
                "id": "s",
                "metadata": metadata,
                "metadataVersion": 1,
                "agentState": {},
                "agentStateVersion": 1
            }),
        )
        .unwrap()
    }

    async fn start(&self) -> (Arc<SessionClient>, mpsc::UnboundedReceiver<SessionEvent>) {
        self.start_with_metadata(json!({"host": "test", "path": "/tmp"}))
            .await
    }
}

async fn wait_connected(events: &mut mpsc::UnboundedReceiver<SessionEvent>) {
    timeout(LIMIT, async {
        while let Some(e) = events.recv().await {
            if matches!(e, SessionEvent::Connected) {
                break;
            }
        }
    })
    .await
    .unwrap();
}

async fn until(condition: impl Fn() -> bool) {
    timeout(LIMIT, async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
}

async fn mcp_call(port: u16, sid: Option<&str>, body: Value) -> (Option<String>, Value) {
    let client = Client::builder(TokioExecutor::new()).build_http::<Full<Bytes>>();
    let mut req = Request::post(format!("http://127.0.0.1:{port}/"))
        .header("content-type", "application/json")
        .header("accept", "application/json");
    if let Some(sid) = sid {
        req = req.header("mcp-session-id", sid);
    }
    let res = client
        .request(req.body(Full::new(Bytes::from(body.to_string()))).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let session = res
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let data = res.collect().await.unwrap().to_bytes();
    (session, serde_json::from_slice(&data).unwrap())
}

async fn init_mcp(port: u16) -> String {
    let (sid, _) = mcp_call(
        port,
        None,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "1.0.0"}
            }
        }),
    )
    .await;
    sid.unwrap()
}

fn decode_replies(raw: &[u8]) -> Vec<Value> {
    let mut replies: Vec<Value> = std::str::from_utf8(raw)
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    replies.sort_by_key(|v| v["id"].as_i64().unwrap_or(i64::MAX));
    replies
}

#[test]
fn goldens_match_typescript() {
    let versions: Value =
        serde_json::from_str(include_str!("../golden/mcp_versions.json")).unwrap();
    assert_eq!(json!(http::VERSIONS), versions["supported"]);
    assert_eq!(http::VERSIONS[0], versions["latest"]);
    let output = std::process::Command::new("bun")
        .current_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/../../cli"))
        .args([
            "--tsconfig-override",
            "tsconfig.json",
            "../crates/hapi-wrapper-core/golden/generate.ts",
            "--check",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn mcp_tools_start_change_title_display_and_skill_lookup() {
    let mut hub = Hub::new().await;
    let (client, mut events) = hub.start().await;
    wait_connected(&mut events).await;

    // Reject empty tool list.
    assert!(tools::start(
        client.clone(),
        Options {
            enabled_tools: Some(vec![]),
            ..Default::default()
        }
    )
    .await
    .is_err());

    let work = tempfile::tempdir().unwrap();
    let skill_dir = work.path().join(".agents/skills/demo-skill");
    fs::create_dir_all(&skill_dir).unwrap();
    fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: demo-skill\ndescription: Demo description\n---\nFollow these steps.\n",
    )
    .unwrap();
    let png_path = work.path().join("diagram.png");
    fs::write(&png_path, b"\x89PNG\r\n\x1a\npayload").unwrap();

    let images = GeneratedImages::default();
    files::register(&client.rpc, work.path(), &images);
    let seen_titles = Arc::new(Mutex::new(Vec::<String>::new()));
    let recorded = seen_titles.clone();

    let server = tools::start(
        client.clone(),
        Options {
            working_directory: Some(work.path().to_path_buf()),
            skill_lookup: Some((work.path().to_path_buf(), "claude".into())),
            generated_images: images.clone(),
            on_change_title: Some(Arc::new(move |t| recorded.lock().unwrap().push(t))),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    assert!(client.flush_metadata(LIMIT).await);
    assert_eq!(
        client.metadata.get().value["hapiMcpUrl"],
        format!("http://127.0.0.1:{}/", server.port)
    );

    let sid = init_mcp(server.port).await;

    // change_title normalizes whitespace, updates hub metadata, and invokes on_change_title.
    let (_, res) = mcp_call(
        server.port,
        Some(&sid),
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {"name": "change_title", "arguments": {"title": "  Fix   login  "}}
        }),
    )
    .await;
    assert_eq!(res["result"]["isError"], false);
    assert_eq!(
        res["result"]["content"][0]["text"],
        "Successfully changed chat title to: \"Fix login\""
    );
    assert_eq!(client.metadata.get().value["name"], "Fix login");
    assert_eq!(*seen_titles.lock().unwrap(), vec!["Fix login"]);

    // Blank title fails cleanly.
    let (_, blank) = mcp_call(
        server.port,
        Some(&sid),
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {"name": "change_title", "arguments": {"title": "   "}}
        }),
    )
    .await;
    assert_eq!(blank["result"]["isError"], true);
    assert!(blank["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("Title must not be empty"));

    // Locked title on hub refuses rename.
    let mut locked_meta = client.metadata.get().value;
    locked_meta["titleLocked"] = true.into();
    let next_ver = client.metadata.get().version + 1;
    hub.command(
        "metadata",
        json!({"metadata": locked_meta, "metadataVersion": next_ver}),
    )
    .await;
    let (_, locked) = mcp_call(
        server.port,
        Some(&sid),
        json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": {"name": "change_title", "arguments": {"title": "Override"}}
        }),
    )
    .await;
    assert_eq!(locked["result"]["isError"], true);
    assert!(locked["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("Session title is locked by the user"));

    // display_image stores into Options.generated_images and emits generated-image message.
    let (_, displayed) = mcp_call(
        server.port,
        Some(&sid),
        json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "tools/call",
            "params": {"name": "display_image", "arguments": {"path": png_path, "title": "Chart"}}
        }),
    )
    .await;
    assert_eq!(displayed["result"]["isError"], false);
    assert_eq!(
        displayed["result"]["content"][0]["text"],
        "Displayed image: Chart.png"
    );
    assert!(client.flush(LIMIT).await);
    let snapshot = hub.command("snapshot", Value::Null).await;
    let emitted = snapshot["emitted"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| {
            e["event"] == "message"
                && e["payload"]["message"]["content"]["data"]["type"] == "generated-image"
        })
        .expect("generated-image message emitted");
    let image_id = emitted["payload"]["message"]["content"]["data"]["imageId"]
        .as_str()
        .unwrap();
    assert_eq!(
        emitted["payload"]["message"]["content"]["data"]["source"],
        json!({"ingress": "mcp", "toolName": "display_image"})
    );
    let raw = client
        .rpc
        .handle_request(
            &json!({"method": "s:readGeneratedImage", "params": json!({"id": image_id}).to_string()}),
        )
        .await
        .unwrap();
    let img_reply: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(img_reply["success"], true);
    assert_eq!(img_reply["fileName"], "Chart.png");
    assert_eq!(img_reply["mimeType"], "image/png");

    // skill_lookup resolves from discovery roots.
    let (_, skill) = mcp_call(
        server.port,
        Some(&sid),
        json!({
            "jsonrpc": "2.0",
            "id": 6,
            "method": "tools/call",
            "params": {"name": "skill_lookup", "arguments": {"name": "demo-skill"}}
        }),
    )
    .await;
    assert_eq!(skill["result"]["isError"], false);
    let skill_text = skill["result"]["content"][0]["text"].as_str().unwrap();
    assert!(skill_text.contains("Skill: demo-skill"));
    assert!(skill_text.contains("Follow these steps."));

    server.shutdown().await;

    // emit_title_summary: false acknowledges without updating metadata.name.
    let mut unlocked_meta = client.metadata.get().value;
    unlocked_meta["titleLocked"] = false.into();
    let next_ver = client.metadata.get().version + 1;
    hub.command(
        "metadata",
        json!({"metadata": unlocked_meta, "metadataVersion": next_ver}),
    )
    .await;
    let server2 = tools::start(
        client.clone(),
        Options {
            emit_title_summary: false,
            enabled_tools: Some(vec!["change_title".into()]),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let sid2 = init_mcp(server2.port).await;
    let (_, quiet) = mcp_call(
        server2.port,
        Some(&sid2),
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {"name": "change_title", "arguments": {"title": "Projected only"}}
        }),
    )
    .await;
    assert_eq!(quiet["result"]["isError"], false);
    assert_eq!(client.metadata.get().value["name"], "Fix login");

    server2.shutdown().await;
    client.close();
}

#[cfg(unix)]
#[tokio::test]
async fn peer_tool_subprocess_contract() {
    use std::os::unix::fs::PermissionsExt;
    if std::env::var_os("HAPI_PEER_CONTRACT_CHILD").is_none() {
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "peer_tool_subprocess_contract"])
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
mode="$(cat "$OUT/mode")"
case "$mode" in
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
    std::env::set_var("HAPI_CLI_EXECUTABLE", &exe);
    std::env::set_var("OUT", root);

    let hub = Hub::new().await;
    let (client, mut events) = hub
        .start_with_metadata(json!({"host": "test", "path": meta_dir}))
        .await;
    wait_connected(&mut events).await;

    let server = tools::start(
        client.clone(),
        Options {
            enabled_tools: Some(vec!["ping_peer".into(), "inspect_peer".into()]),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let sid = init_mcp(server.port).await;

    // Exit 0 success, stale env cleared, argv + stdin verified, cwd from metadata.path.
    fs::write(root.join("mode"), "ok").unwrap();
    let (_, ok) = mcp_call(
        server.port,
        Some(&sid),
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {"name": "ping_peer", "arguments": {"sessionIdPrefix": " abc ", "message": "hello world"}}
        }),
    )
    .await;
    assert_eq!(
        ok["result"],
        json!({"content":[{"type":"text","text":"ok"}],"isError":false})
    );
    assert_eq!(
        fs::read_to_string(root.join("argv")).unwrap(),
        "peer-tool\nping_peer\n"
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&fs::read(root.join("call.stdin")).unwrap()).unwrap(),
        json!({"sessionIdPrefix": "abc", "message": "hello world"})
    );
    assert_eq!(
        fs::read_to_string(root.join("identity")).unwrap(),
        format!(
            "s\nunset\nunset\n{}\n",
            fs::canonicalize(&meta_dir).unwrap().display()
        )
    );

    // Exit 1 with valid MCP JSON `isError: true` preserves the tool error verbatim.
    fs::write(root.join("mode"), "tool_error").unwrap();
    let (_, tool_err) = mcp_call(
        server.port,
        Some(&sid),
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {"name": "inspect_peer", "arguments": {"sessionIdPrefix": "nope"}}
        }),
    )
    .await;
    assert_eq!(
        tool_err["result"],
        json!({"content":[{"type":"text","text":"Failed to inspect peer: not found"}],"isError":true})
    );

    // Exit 1 crash, exit 2 usage error, and malformed exit 0 surface as isError: true.
    for (id, mode, expected) in [
        (4, "crash", "stub crash"),
        (5, "usage", "unknown peer tool"),
        (6, "malformed", "Invalid peer-tool response"),
    ] {
        fs::write(root.join("mode"), mode).unwrap();
        let (_, res) = mcp_call(
            server.port,
            Some(&sid),
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "tools/call",
                "params": {"name": "inspect_peer", "arguments": {"sessionIdPrefix": "x"}}
            }),
        )
        .await;
        assert_eq!(res["result"]["isError"], true);
        assert!(res["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains(expected));
    }
    server.shutdown().await;

    // Cwd precedence (working_directory > skill_lookup > metadata.path) + parent identity env.
    let hub2 = Hub::new().await;
    let (client2, mut events2) = hub2
        .start_with_metadata(json!({
            "host": "test",
            "path": meta_dir,
            "flavor": "codex",
            "codexSessionId": " native ",
            "name": "Parent"
        }))
        .await;
    wait_connected(&mut events2).await;
    fs::write(root.join("mode"), "ok").unwrap();
    for (working, skill_dir, expected) in [
        (Some(work.as_path()), Some(skill.as_path()), work.as_path()),
        (None, Some(skill.as_path()), skill.as_path()),
        (None, None, meta_dir.as_path()),
    ] {
        let srv = tools::start(
            client2.clone(),
            Options {
                working_directory: working.map(Into::into),
                skill_lookup: skill_dir.map(|s| (s.into(), "claude".into())),
                enabled_tools: Some(vec!["inspect_peer".into()]),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let sid = init_mcp(srv.port).await;
        let (_, res) = mcp_call(
            srv.port,
            Some(&sid),
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {"name": "inspect_peer", "arguments": {"sessionIdPrefix": "x"}}
            }),
        )
        .await;
        assert_eq!(res["result"]["isError"], false);
        assert_eq!(
            fs::read_to_string(root.join("identity")).unwrap(),
            format!(
                "s\nParent\nnative\n{}\n",
                fs::canonicalize(expected).unwrap().display()
            )
        );
        srv.shutdown().await;
    }

    // Closing the session kills an in-flight peer-tool subprocess.
    fs::write(root.join("mode"), "block").unwrap();
    let srv = tools::start(
        client2.clone(),
        Options {
            enabled_tools: Some(vec!["inspect_peer".into()]),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let sid = init_mcp(srv.port).await;
    let port = srv.port;
    let call = tokio::spawn(async move {
        mcp_call(
            port,
            Some(&sid),
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {"name": "inspect_peer", "arguments": {"sessionIdPrefix": "x"}}
            }),
        )
        .await
    });
    let pid_file = root.join("pid");
    until(|| fs::read_to_string(&pid_file).is_ok_and(|s| s.trim().parse::<u32>().is_ok())).await;
    let pid = fs::read_to_string(&pid_file).unwrap();
    let proc = format!("/proc/{}", pid.trim());
    client2.close();
    let (_, cancelled) = call.await.unwrap();
    assert_eq!(cancelled["result"]["isError"], true);
    until(|| !Path::new(&proc).exists()).await;

    // HAPI_DISABLE_PEER_TOOLS removes peer tools from start() and refuses calls.
    fs::remove_file(root.join("call.stdin")).ok();
    std::env::set_var("HAPI_DISABLE_PEER_TOOLS", "YES");
    assert!(tools::start(
        client.clone(),
        Options {
            enabled_tools: Some(vec!["inspect_peer".into()]),
            ..Default::default()
        },
    )
    .await
    .is_err());
    assert!(!root.join("call.stdin").exists());

    srv.shutdown().await;
    client.close();
}

#[tokio::test]
async fn stdio_bridge_typescript_parity() {
    if let Ok(raw_args) = std::env::var("HAPI_STDIO_BRIDGE_TEST_ARGS") {
        let args: Vec<String> = serde_json::from_str(&raw_args).unwrap();
        // Separate stdio_bridge's JSON-RPC lines from libtest's unterminated status prefix.
        println!();
        stdio_bridge::run(&args).await.unwrap();
        return;
    }

    let messages = [
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "clientInfo": {"name": "test", "version": "1.0.0"},
                "capabilities": {}
            }
        }),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}),
    ];
    let mut payload = String::from("  \n");
    for msg in &messages {
        payload.push_str(&msg.to_string());
        payload.push('\n');
    }
    let payload_bytes = payload.into_bytes();
    let all = "change_title,display_image,display_video,display_media,list_peers,ping_peer,inspect_peer,spawn_peer,session_job,skill_lookup";
    let exe = std::env::current_exe().unwrap();

    for (disabled, tools) in [
        (false, None),
        (false, Some(all)),
        (true, Some(all)),
        (false, Some("display_media,skill_lookup")),
    ] {
        let args: Vec<String> = match tools {
            None => vec![],
            Some(list) => vec![
                "--tools".into(),
                list.into(),
                "--hapi-session-id=stamp".into(),
            ],
        };

        let mut rust_child = Command::new(&exe)
            .args(["--exact", "stdio_bridge_typescript_parity", "--nocapture"])
            .env(
                "HAPI_STDIO_BRIDGE_TEST_ARGS",
                serde_json::to_string(&args).unwrap(),
            )
            .env("HAPI_DISABLE_PEER_TOOLS", if disabled { "1" } else { "0" })
            .env("HAPI_HTTP_MCP_URL", "http://127.0.0.1:1/")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut rust_stdin = rust_child.stdin.take().unwrap();
        let bytes = payload_bytes.clone();
        let (_, rust_out) = tokio::join!(
            async move { rust_stdin.write_all(&bytes).await.unwrap() },
            rust_child.wait_with_output()
        );
        let rust_out = rust_out.unwrap();
        assert!(
            rust_out.status.success(),
            "{}",
            String::from_utf8_lossy(&rust_out.stderr)
        );
        // Filter out libtest header/footer lines, keeping only JSON-RPC replies.
        let rust_json: Vec<u8> = std::str::from_utf8(&rust_out.stdout)
            .unwrap()
            .lines()
            .filter(|l| l.starts_with('{'))
            .collect::<Vec<_>>()
            .join("\n")
            .into_bytes();
        let actual = decode_replies(&rust_json);

        let script = format!(
            "import {{runHappyMcpStdioBridge as run}} from './src/codex/happyMcpStdioBridge';await run({})",
            serde_json::to_string(&args).unwrap()
        );
        let mut ts_child = Command::new("bun")
            .current_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/../../cli"))
            .args(["--tsconfig-override", "tsconfig.json", "-e", &script])
            .env("HAPI_DISABLE_PEER_TOOLS", if disabled { "1" } else { "0" })
            .env("HAPI_HTTP_MCP_URL", "http://127.0.0.1:1/")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut ts_stdin = ts_child.stdin.take().unwrap();
        let bytes = payload_bytes.clone();
        let (_, ts_out) = tokio::join!(
            async move { ts_stdin.write_all(&bytes).await.unwrap() },
            ts_child.wait_with_output()
        );
        let ts_out = ts_out.unwrap();
        assert!(
            ts_out.status.success(),
            "{}",
            String::from_utf8_lossy(&ts_out.stderr)
        );
        let expected = decode_replies(&ts_out.stdout);
        assert_eq!(actual, expected);
    }
}
