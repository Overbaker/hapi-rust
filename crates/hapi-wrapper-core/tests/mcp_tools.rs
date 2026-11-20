#![cfg(unix)]

use bytes::Bytes;
use hapi_wrapper_core::{
    config::Config,
    mcp::tools::{self, Options},
    rest::Rest,
    rpc::handlers::{files, GeneratedImages},
    session::client::{SessionClient, SessionEvent},
};
use http_body_util::{BodyExt, Full};
use hyper::Request;
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
use serde_json::{json, Value};
use std::{
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

    async fn start(&self) -> (Arc<SessionClient>, mpsc::UnboundedReceiver<SessionEvent>) {
        SessionClient::start(
            &self.config,
            Rest::new(&self.config).await.unwrap(),
            &json!({
                "id": "s",
                "metadata": {"host": "test", "path": "/tmp"},
                "metadataVersion": 1,
                "agentState": {},
                "agentStateVersion": 1
            }),
        )
        .unwrap()
    }
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

#[tokio::test]
async fn mcp_tools_start_change_title_display_and_skill_lookup() {
    let mut hub = Hub::new().await;
    let (client, mut events) = hub.start().await;
    timeout(LIMIT, async {
        while let Some(e) = events.recv().await {
            if matches!(e, SessionEvent::Connected) {
                break;
            }
        }
    })
    .await
    .unwrap();

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
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: demo-skill\ndescription: Demo description\n---\nFollow these steps.\n",
    )
    .unwrap();
    let png_path = work.path().join("diagram.png");
    std::fs::write(&png_path, b"\x89PNG\r\n\x1a\npayload").unwrap();

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
        .handle_request(&json!({"method": "s:readGeneratedImage", "params": json!({"id": image_id}).to_string()}))
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
