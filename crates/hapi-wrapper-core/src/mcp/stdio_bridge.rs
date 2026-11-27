//! STDIO-to-Streamable-HTTP MCP bridge for Codex (`hapi-session-wrapper mcp`).
//!
//! # Known gaps
//!
//! - Each forwarded `tools/call` uses a fixed 60 s timeout and does not forward STDIO
//!   `notifications/cancelled` to the upstream HTTP MCP server.
//! - Lazy HTTP session initialization caches a single `mcp-session-id` for the lifetime
//!   of the bridge (no automatic re-init if the upstream HTTP MCP server drops the session).

use super::{
    http::VERSIONS,
    link_pr,
    tools::{self, Options},
};
use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::{header::HeaderValue, Request, Uri};
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::{
    client::legacy::{connect::HttpConnector, Client},
    rt::TokioExecutor,
};
use serde_json::{json, Value};
use std::{
    io,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader},
    sync::OnceCell,
    task::JoinSet,
};

pub fn catalog(names: Option<Vec<String>>) -> Value {
    catalog_with(names, tools::disabled())
}

fn catalog_with(names: Option<Vec<String>>, disabled: bool) -> Value {
    tools::catalog(
        &Options {
            skill_lookup: Some((Default::default(), "codex".into())),
            enabled_tools: names,
            ..Default::default()
        },
        disabled,
    )
}

enum Transport {
    Http(Client<HttpConnector, Full<Bytes>>),
    Https(Client<HttpsConnector<HttpConnector>, Full<Bytes>>),
}

struct HttpSession {
    client: Transport,
    sid: Option<HeaderValue>,
    version: String,
}

struct Bridge {
    url: Uri,
    session: OnceCell<HttpSession>,
    next: AtomicU64,
    tools: Value,
    link_pr: Option<link_pr::Visibility>,
}

async fn post(
    client: &Transport,
    url: &Uri,
    sid: Option<&HeaderValue>,
    version: Option<&str>,
    v: Value,
) -> io::Result<(Option<HeaderValue>, Value)> {
    let mut r = Request::post(url)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream");
    if let Some(id) = sid {
        r = r.header("mcp-session-id", id);
    }
    if let Some(ver) = version {
        r = r.header("mcp-protocol-version", ver);
    }
    let packet = r
        .body(Full::new(Bytes::from(v.to_string())))
        .map_err(io::Error::other)?;
    let res = match client {
        Transport::Http(c) => c.request(packet).await,
        Transport::Https(c) => c.request(packet).await,
    }
    .map_err(io::Error::other)?;
    if !res.status().is_success() {
        return Err(io::Error::other(format!("HTTP {}", res.status())));
    }
    let sid = res.headers().get("mcp-session-id").cloned();
    if v.get("id").is_none() {
        return Ok((sid, Value::Null));
    }
    let sse = res
        .headers()
        .get("content-type")
        .is_some_and(|h| h.as_bytes().starts_with(b"text/event-stream"));
    let body = Limited::new(res.into_body(), 64 << 20);
    let response: Value = if sse {
        sse_response(body, &v["id"]).await?
    } else {
        serde_json::from_slice(&body.collect().await.map_err(io::Error::other)?.to_bytes())?
    };
    if response["id"] != v["id"] {
        return Err(io::Error::other("Unexpected MCP response ID"));
    }
    if response.get("error").is_some() {
        return Err(io::Error::other(
            response["error"]["message"]
                .as_str()
                .unwrap_or("MCP request failed")
                .to_owned(),
        ));
    }
    Ok((sid, response["result"].clone()))
}

#[derive(Default)]
struct SseEvent {
    kind: String,
    data: String,
}

impl SseEvent {
    fn line(&mut self, line: &[u8], id: &Value) -> io::Result<Option<Value>> {
        let field = std::str::from_utf8(line)
            .map_err(io::Error::other)?
            .trim_end_matches(['\r', '\n']);
        if field.is_empty() {
            let event = std::mem::take(self);
            if (event.kind.is_empty() || event.kind == "message") && !event.data.trim().is_empty() {
                let value: Value = serde_json::from_str(&event.data)?;
                if value.get("id") == Some(id) {
                    return Ok(Some(value));
                }
            }
        } else if let Some(kind) = field.strip_prefix("event:") {
            self.kind = kind.strip_prefix(' ').unwrap_or(kind).to_owned();
        } else if let Some(data) = field.strip_prefix("data:") {
            self.data.push_str(data.strip_prefix(' ').unwrap_or(data));
            self.data.push('\n');
        }
        Ok(None)
    }
}

async fn sse_response(mut body: Limited<hyper::body::Incoming>, id: &Value) -> io::Result<Value> {
    let mut event = SseEvent::default();
    let mut line = Vec::new();
    while let Some(frame) = body.frame().await {
        let Ok(bytes) = frame.map_err(io::Error::other)?.into_data() else {
            continue;
        };
        for segment in bytes.split_inclusive(|b| *b == b'\n') {
            line.extend_from_slice(segment);
            if segment.ends_with(b"\n") {
                if let Some(value) = event.line(&line, id)? {
                    return Ok(value);
                }
                line.clear();
            }
        }
    }
    if !line.is_empty() {
        event.line(&line, id)?;
    }
    event
        .line(b"", id)?
        .ok_or_else(|| io::Error::other("Missing MCP response"))
}

impl Bridge {
    async fn forward(&self, method: &str, params: Value) -> io::Result<Value> {
        let session = self
            .session
            .get_or_try_init(|| async {
                let builder = Client::builder(TokioExecutor::new());
                let client = match self.url.scheme_str() {
                    Some("http") => Transport::Http(builder.build_http()),
                    Some("https") => {
                        let connector = HttpsConnectorBuilder::new()
                            .with_tls_config((*crate::sio::eio::tls_config().await?).clone())
                            .https_only()
                            .enable_http1()
                            .build();
                        Transport::Https(builder.build(connector))
                    }
                    _ => return Err(io::Error::other("Expected HTTP or HTTPS MCP URL")),
                };
                let (sid, res) = post(
                    &client,
                    &self.url,
                    None,
                    None,
                    json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":VERSIONS[0],"clientInfo":{"name":"hapi-stdio-bridge","version":"1.0.0"},"capabilities":{}}}),
                )
                .await?;
                let version = res["protocolVersion"]
                    .as_str()
                    .filter(|s| VERSIONS.contains(s))
                    .ok_or_else(|| io::Error::other("Unsupported MCP protocol version"))?
                    .to_owned();
                post(
                    &client,
                    &self.url,
                    sid.as_ref(),
                    Some(&version),
                    json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
                )
                .await?;
                Ok::<_, io::Error>(HttpSession {
                    client,
                    sid,
                    version,
                })
            })
            .await?;
        let (_, result) = post(
            &session.client,
            &self.url,
            session.sid.as_ref(),
            Some(&session.version),
            json!({"jsonrpc":"2.0","method":method,"id":self.next.fetch_add(1,Ordering::Relaxed),"params":params}),
        )
        .await?;
        Ok(result)
    }

    async fn refresh_link_pr(&self) -> bool {
        match &self.link_pr {
            Some(visibility) => {
                visibility
                    .check(async {
                        let catalog = tokio::time::timeout(
                            Duration::from_secs(10),
                            self.forward("tools/list", json!({})),
                        )
                        .await
                        .ok()?
                        .ok()?;
                        Some(link_pr::listed(&catalog))
                    })
                    .await
            }
            None => false,
        }
    }

    async fn handle(&self, v: Value) -> Option<Value> {
        let id = v.get("id")?;
        let reply = |r: Value| json!({"jsonrpc":"2.0","id":id,"result":r});
        let err =
            |code: i32, s: &str| json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":s}});
        let params = &v["params"];
        let Some(method) = v["method"].as_str() else {
            return Some(err(-32600, "Invalid Request"));
        };
        let link_pr_enabled = method != "initialize" && self.refresh_link_pr().await;
        Some(match method {
            "initialize" => reply(
                json!({"protocolVersion":params["protocolVersion"].as_str().filter(|v|VERSIONS.contains(v)).unwrap_or(VERSIONS[0]),"serverInfo":{"name":"HAPI MCP Bridge","version":"1.0.0"},"capabilities":{"tools":{"listChanged":true}}}),
            ),
            "tools/list" => {
                let mut catalog = self.tools.clone();
                if !link_pr_enabled {
                    link_pr::hide(&mut catalog);
                }
                reply(catalog)
            }
            "ping" => reply(json!({})),
            "tools/call" => {
                let name = params["name"].as_str().unwrap_or("");
                let tool = self.tools["tools"]
                    .as_array()?
                    .iter()
                    .find(|t| t["name"] == name);
                let disabled = tool.is_some() && name == link_pr::NAME && !link_pr_enabled;
                let Some(tool) = tool.filter(|_| !disabled) else {
                    let reason = if disabled { "disabled" } else { "not found" };
                    return Some(reply(tools::result(Err(format!(
                        "MCP error -32602: Tool {name} {reason}"
                    )))));
                };
                let result = match tools::validate(
                    tool,
                    params
                        .get("arguments")
                        .filter(|v| !v.is_null())
                        .cloned()
                        .unwrap_or(json!({})),
                ) {
                    Ok(args) => tokio::time::timeout(
                        Duration::from_secs(60),
                        self.forward("tools/call", json!({"name":name,"arguments":args})),
                    )
                    .await
                    .map_err(io::Error::other)
                    .and_then(|r| r)
                    .unwrap_or_else(|e| {
                        tools::result(Err(format!(
                            "Failed to {}: {e}",
                            match name {
                                "change_title" => "change chat title".into(),
                                "session_job" => "run session_job".into(),
                                "skill_lookup" => "look up skill".into(),
                                link_pr::NAME => "link PR".into(),
                                _ => name.replace('_', " "),
                            }
                        )))
                    }),
                    Err(e) => tools::result(Err(e)),
                };
                reply(result)
            }
            _ => err(-32601, "Method not found"),
        })
    }
}

pub async fn run(args: &[String]) -> io::Result<()> {
    run_io(
        args,
        tokio::io::stdin(),
        tokio::io::stdout(),
        std::env::var("HAPI_HTTP_MCP_URL").ok(),
        tools::disabled(),
    )
    .await
}

async fn run_io<R, W>(
    args: &[String],
    input: R,
    mut output: W,
    env_url: Option<String>,
    disabled: bool,
) -> io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut args = args.iter();
    let mut url = None;
    let mut names = None;
    while let Some(key) = args.next() {
        match key.as_str() {
            "--url" => url = args.next().cloned(),
            "--tools" => {
                names = args.next().map(|s| {
                    s.split(',')
                        .map(str::trim)
                        .filter(|v| !v.is_empty())
                        .map(str::to_owned)
                        .collect()
                })
            }
            "--hapi-session-id" => {
                args.next();
            }
            _ => {}
        }
    }
    let url = url
        .filter(|v| !v.is_empty())
        .or(env_url)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            io::Error::other(
                "Missing target URL. Set HAPI_HTTP_MCP_URL or pass --url <http://127.0.0.1:PORT>",
            )
        })?
        .parse::<Uri>()
        .map_err(io::Error::other)?;
    let default = names.is_none();
    let mut list = catalog_with(names, disabled);
    list["tools"].as_array_mut().unwrap().retain_mut(|t| {
        match t["name"].as_str() {
            Some("change_title") => {
                t["description"] = json!("Change the title of the current chat session")
            }
            Some("spawn_peer") => {
                t["inputSchema"]["properties"]["sessionType"]["description"] = json!("simple or worktree. Default simple (use directory as cwd). worktree creates a new tree from directory.")
            }
            Some("skill_lookup" | link_pr::NAME) if default => return false,
            Some(link_pr::NAME) => {
                t["description"] = json!("Attach the current HAPI session to a GitHub pull request");
                let props = &mut t["inputSchema"]["properties"];
                props["url"]["description"] = json!("GitHub PR URL");
                props["repo"]["description"] = json!("owner/repo");
                props["number"]["description"] = json!("PR number");
                props["role"].as_object_mut().unwrap().shift_remove("description");
            }
            _ => {}
        }
        true
    });
    let link_pr = link_pr::listed(&list).then(link_pr::Visibility::default);
    let mut changes = link_pr.as_ref().map(link_pr::Visibility::subscribe);
    let bridge = Arc::new(Bridge {
        url,
        session: OnceCell::new(),
        next: AtomicU64::new(1),
        tools: list,
        link_pr,
    });
    let mut poll = tokio::time::interval_at(
        tokio::time::Instant::now() + link_pr::POLL_INTERVAL,
        link_pr::POLL_INTERVAL,
    );
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut lines = BufReader::new(input).lines();
    let mut jobs = JoinSet::new();
    let mut initialized = false;
    let mut eof = false;
    while !eof || !jobs.is_empty() {
        tokio::select! {
            _ = poll.tick(), if initialized && changes.is_some() && !eof => {
                let bridge = bridge.clone();
                jobs.spawn(async move {
                    bridge.refresh_link_pr().await;
                    None
                });
            }
            changed = async { changes.as_mut().unwrap().changed().await }, if initialized && changes.is_some() => {
                changed.map_err(io::Error::other)?;
                output.write_all(format!("{}\n", link_pr::LIST_CHANGED).as_bytes()).await?;
                output.flush().await?;
            }
            line = lines.next_line(), if !eof => match line? {
                Some(s) if s.trim().is_empty() => continue,
                Some(s) => {
                    let packet = serde_json::from_str::<Value>(&s);
                    if !initialized && packet.as_ref().is_ok_and(|v| {
                        v["jsonrpc"] == "2.0" && v["method"] == "notifications/initialized" && v.get("id").is_none()
                    }) {
                        initialized = true;
                        poll.reset();
                        continue;
                    }
                    let bridge = bridge.clone();
                    jobs.spawn(async move {
                        match packet {
                            Ok(v) => bridge.handle(v).await,
                            Err(_) => Some(json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"Parse error"}}))
                        }
                    });
                }
                None => eof = true,
            },
            job = jobs.join_next(), if !jobs.is_empty() => {
                if let Some(packet) = job.unwrap().map_err(io::Error::other)? {
                    output.write_all(format!("{packet}\n").as_bytes()).await?;
                    output.flush().await?;
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        loopback::{reply, Listener},
        mcp::http::{self, Tools},
    };
    use futures_util::future::BoxFuture;
    use std::sync::Mutex;

    fn encode_packets(packets: &[Value]) -> Vec<u8> {
        let mut raw = String::from("  \n");
        for packet in packets {
            raw.push_str(&packet.to_string());
            raw.push('\n');
        }
        raw.into_bytes()
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

    fn init_messages() -> Vec<Value> {
        vec![
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
        ]
    }

    #[tokio::test]
    async fn missing_url_errors() {
        let mut out = Vec::new();
        let err = run_io(
            &[],
            std::io::Cursor::new(Vec::<u8>::new()),
            &mut out,
            None,
            false,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("Missing target URL"));
    }

    #[tokio::test]
    async fn link_pr_waits_for_stdio_initialization_before_notifying() {
        struct Enabled;
        impl Tools for Enabled {
            fn list(&self) -> Value {
                json!({"tools":[{"name":"link_pr","inputSchema":{"type":"object"}}]})
            }
            fn call(&self, _: String, _: Value) -> BoxFuture<'_, Value> {
                unreachable!()
            }
            fn link_pr_awareness(&self) -> BoxFuture<'_, bool> {
                Box::pin(async { true })
            }
        }
        let server = http::serve(Arc::new(Enabled)).await.unwrap();
        let url = format!("http://127.0.0.1:{}/", server.port);
        let (client, bridge) = tokio::io::duplex(4096);
        let (input, output) = tokio::io::split(bridge);
        let run = tokio::spawn(async move {
            run_io(
                &["--tools".into(), "link_pr".into()],
                input,
                output,
                Some(url),
                false,
            )
            .await
        });
        let (read, mut write) = tokio::io::split(client);
        let mut lines = BufReader::new(read).lines();
        tokio::task::yield_now().await;
        tokio::time::pause();
        tokio::time::advance(link_pr::POLL_INTERVAL + Duration::from_secs(1)).await;
        tokio::time::resume();
        assert!(
            tokio::time::timeout(Duration::from_millis(200), lines.next_line())
                .await
                .is_err()
        );

        let packets = init_messages();
        write
            .write_all(&encode_packets(&packets[..1]))
            .await
            .unwrap();
        let init = tokio::time::timeout(Duration::from_secs(5), lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(serde_json::from_str::<Value>(&init).unwrap()["id"], 1);
        write
            .write_all(&encode_packets(&packets[1..]))
            .await
            .unwrap();
        let replies = tokio::time::timeout(Duration::from_secs(5), async {
            [
                lines.next_line().await.unwrap().unwrap(),
                lines.next_line().await.unwrap().unwrap(),
            ]
        })
        .await
        .unwrap();
        assert!(replies.iter().any(|line| line == link_pr::LIST_CHANGED));
        assert!(replies
            .iter()
            .any(|line| link_pr::listed(&serde_json::from_str::<Value>(line).unwrap()["result"])));
        write.shutdown().await.unwrap();
        run.await.unwrap().unwrap();
        server.shutdown().await;
    }

    #[tokio::test]
    async fn forwards_calls_over_sse_with_lazy_session_and_handles_errors() {
        let calls = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen = calls.clone();
        let server = Listener::bind(move |req, _, _| {
            let seen = seen.clone();
            async move {
                let (parts, body) = req.into_parts();
                let bytes = body.collect().await.unwrap().to_bytes();
                let v: Value = serde_json::from_slice(&bytes).unwrap();
                let method = v["method"].as_str().unwrap().to_owned();
                seen.lock().unwrap().push(method.clone());
                assert!(parts.headers["accept"]
                    .to_str()
                    .unwrap()
                    .contains("text/event-stream"));
                if method == "initialize" {
                    let mut res = reply(
                        200,
                        "application/json",
                        json!({
                            "jsonrpc": "2.0",
                            "id": v["id"],
                            "result": {
                                "protocolVersion": "2025-11-25",
                                "capabilities": {"tools": {}},
                                "serverInfo": {"name": "upstream", "version": "1"}
                            }
                        })
                        .to_string(),
                    );
                    res.headers_mut()
                        .insert("mcp-session-id", "example".parse().unwrap());
                    return res;
                }
                assert_eq!(parts.headers["mcp-session-id"], "example");
                assert_eq!(parts.headers["mcp-protocol-version"], "2025-11-25");
                if method == "notifications/initialized" {
                    return reply(202, "text/plain", "");
                }
                let empty = v["params"]["name"] == "list_peers";
                if empty {
                    assert_eq!(v["params"]["arguments"], json!({}));
                } else {
                    assert_eq!(v["params"]["name"], "display_image");
                }
                let data = format!(
                    "data: {}",
                    json!({
                        "jsonrpc": "2.0",
                        "id": v["id"],
                        "result": {"content": [{"type": "text", "text": if empty { "empty" } else { v["params"]["arguments"]["path"].as_str().unwrap() }}]}
                    })
                );
                let payload = format!(
                    "id: 0\ndata:\n\nevent: ping\ndata: ok\n\n{data}{}",
                    if empty { "" } else { "\n\n" }
                );
                reply(200, "text/event-stream", payload)
            }
        })
        .await
        .unwrap();

        let mut packets = init_messages();
        packets.extend([
            json!({"jsonrpc": "2.0", "id": 5, "method": "tools/call", "params": {"name": "list_peers", "arguments": null}}),
            json!({"jsonrpc": "2.0", "id": 6}),
            json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "display_image", "arguments": {"path": "/image-3.png"}}}),
            json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {"name": "display_image", "arguments": {"path": "/image-4.png"}}}),
            json!({"jsonrpc": "2.0", "id": 7, "method": "tools/call", "params": {"name": "missing"}}),
            json!({"jsonrpc": "2.0", "id": 8, "method": "unknown/method"}),
        ]);
        let mut input = encode_packets(&packets);
        input.extend_from_slice(b"not-json\n");
        let args = vec![
            "--tools".into(),
            "display_image,list_peers".into(),
            "--url".into(),
            format!("http://127.0.0.1:{}/", server.port),
            "--hapi-session-id".into(),
            "sid".into(),
        ];
        let mut out = Vec::new();
        run_io(&args, std::io::Cursor::new(input), &mut out, None, false)
            .await
            .unwrap();
        let replies = decode_replies(&out);
        assert_eq!(
            *calls.lock().unwrap(),
            vec![
                "initialize",
                "notifications/initialized",
                "tools/call",
                "tools/call",
                "tools/call",
            ]
        );
        let by_id = |id: i64| replies.iter().find(|v| v["id"] == id).unwrap();
        assert_eq!(by_id(3)["result"]["content"][0]["text"], "/image-3.png");
        assert_eq!(by_id(4)["result"]["content"][0]["text"], "/image-4.png");
        assert_eq!(by_id(5)["result"]["content"][0]["text"], "empty");
        assert_eq!(by_id(6)["error"]["code"], -32600);
        assert_eq!(by_id(7)["result"]["isError"], true);
        assert!(by_id(7)["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Tool missing not found"));
        assert_eq!(by_id(8)["error"]["code"], -32601);
        assert!(replies
            .iter()
            .any(|v| v["id"].is_null() && v["error"]["code"] == -32700));
    }

    #[tokio::test]
    async fn bridges_to_mcp_http_server() {
        struct Stub;
        impl Tools for Stub {
            fn list(&self) -> Value {
                json!({"tools":[{"name":"change_title","inputSchema":{"type":"object"}}]})
            }
            fn call(&self, name: String, args: Value) -> BoxFuture<'_, Value> {
                Box::pin(async move {
                    json!({"content":[{"type":"text","text":format!("{name}:{}", args["title"].as_str().unwrap())}],"isError":false})
                })
            }
        }
        let server = http::serve(Arc::new(Stub)).await.unwrap();
        let mut packets = init_messages();
        packets.push(json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {"name": "change_title", "arguments": {"title": "Ship it"}}
        }));
        let args = vec![
            "--tools".into(),
            "change_title".into(),
            "--url".into(),
            format!("http://127.0.0.1:{}/", server.port),
        ];
        let mut out = Vec::new();
        run_io(
            &args,
            std::io::Cursor::new(encode_packets(&packets)),
            &mut out,
            None,
            false,
        )
        .await
        .unwrap();
        let replies = decode_replies(&out);
        let call = replies.iter().find(|v| v["id"] == 3).unwrap();
        assert_eq!(call["result"]["isError"], false);
        assert_eq!(call["result"]["content"][0]["text"], "change_title:Ship it");
        server.shutdown().await;
    }
}
