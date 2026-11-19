use super::{
    http::VERSIONS,
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
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::OnceCell,
    task::JoinSet,
};

pub fn catalog(names: Option<Vec<String>>) -> Value {
    tools::catalog(
        &Options {
            skill_lookup: Some((Default::default(), "codex".into())),
            enabled_tools: names,
            ..Default::default()
        },
        tools::disabled(),
    )
}
struct HttpSession {
    client: Client<HttpsConnector<HttpConnector>, Full<Bytes>>,
    sid: Option<HeaderValue>,
    version: String,
}
struct Bridge {
    url: Uri,
    session: OnceCell<HttpSession>,
    next: AtomicU64,
    tools: Value,
}
async fn post(
    client: &Client<HttpsConnector<HttpConnector>, Full<Bytes>>,
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
    let res = client.request(packet).await.map_err(io::Error::other)?;
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
async fn sse_response(mut body: Limited<hyper::body::Incoming>, id: &Value) -> io::Result<Value> {
    let mut line = Vec::new();
    let mut data = String::new();
    while let Some(frame) = body.frame().await {
        let Ok(bytes) = frame.map_err(io::Error::other)?.into_data() else {
            continue;
        };
        for segment in bytes.split_inclusive(|b| *b == b'\n') {
            line.extend_from_slice(segment);
            if !segment.ends_with(b"\n") {
                continue;
            }
            let field = std::str::from_utf8(&line)
                .map_err(io::Error::other)?
                .trim_end_matches(['\r', '\n']);
            if field.is_empty() {
                if !data.is_empty() {
                    let v: Value = serde_json::from_str(&data)?;
                    if v.get("id") == Some(id) {
                        return Ok(v);
                    }
                    data.clear();
                }
            } else if let Some(value) = field.strip_prefix("data:") {
                data.push_str(value.strip_prefix(' ').unwrap_or(value));
                data.push('\n');
            }
            line.clear();
        }
    }
    Err(io::Error::other("Missing MCP response"))
}
impl Bridge {
    async fn forward(&self, params: Value) -> io::Result<Value> {
        let session = self
            .session
            .get_or_try_init(|| async {
                let connector = HttpsConnectorBuilder::new().with_tls_config((*crate::sio::eio::tls_config().await?).clone()).https_or_http().enable_http1().build();
                let client = Client::builder(TokioExecutor::new()).build(connector);
                let (sid, res) = post(&client, &self.url, None, None, json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":VERSIONS[0],"clientInfo":{"name":"hapi-stdio-bridge","version":"1.0.0"},"capabilities":{}}})).await?;
                let version = res["protocolVersion"].as_str().filter(|s| VERSIONS.contains(s)).ok_or_else(|| io::Error::other("Unsupported MCP protocol version"))?.to_owned();
                post(&client, &self.url, sid.as_ref(), Some(&version), json!({"jsonrpc":"2.0","method":"notifications/initialized"})).await?;
                Ok::<_, io::Error>(HttpSession {
                    client,
                    sid,
                    version,
                })
            })
            .await?;
        let (_, result) = post(&session.client, &self.url, session.sid.as_ref(), Some(&session.version), json!({"jsonrpc":"2.0","method":"tools/call","id":self.next.fetch_add(1,Ordering::Relaxed),"params":params})).await?;
        Ok(result)
    }
    async fn handle(&self, v: Value) -> Option<Value> {
        let id = v.get("id")?;
        let reply = |r: Value| json!({"jsonrpc":"2.0","id":id,"result":r});
        let err =
            |code: i32, s: &str| json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":s}});
        let params = &v["params"];
        Some(match v["method"].as_str()? {
            "initialize" => reply(
                json!({"protocolVersion":params["protocolVersion"].as_str().filter(|v|VERSIONS.contains(v)).unwrap_or(VERSIONS[0]),"serverInfo":{"name":"HAPI MCP Bridge","version":"1.0.0"},"capabilities":{"tools":{"listChanged":true}}}),
            ),
            "tools/list" => reply(self.tools.clone()),
            "ping" => reply(json!({})),
            "tools/call" => {
                let name = params["name"].as_str().unwrap_or("");
                let tool = self.tools["tools"]
                    .as_array()?
                    .iter()
                    .find(|t| t["name"] == name);
                let Some(tool) = tool else {
                    return Some(reply(tools::result(Err(format!(
                        "MCP error -32602: Tool {name} not found"
                    )))));
                };
                let result = match tools::validate(
                    tool,
                    params.get("arguments").cloned().unwrap_or(json!({})),
                ) {
                    Ok(args) => tokio::time::timeout(
                        Duration::from_secs(60),
                        self.forward(json!({"name":name,"arguments":args})),
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
        .or_else(|| std::env::var("HAPI_HTTP_MCP_URL").ok())
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            io::Error::other(
                "Missing target URL. Set HAPI_HTTP_MCP_URL or pass --url <http://127.0.0.1:PORT>",
            )
        })?
        .parse::<Uri>()
        .map_err(io::Error::other)?;
    let default = names.is_none();
    let mut list = catalog(names);
    list["tools"].as_array_mut().unwrap().retain_mut(|t| {
        match t["name"].as_str() {
            Some("change_title") => t["description"] = json!("Change the title of the current chat session"),
            Some("spawn_peer") => t["inputSchema"]["properties"]["sessionType"]["description"] = json!("simple or worktree. Default simple (use directory as cwd). worktree creates a new tree from directory."),
            Some("skill_lookup") if default => return false,
            _ => {}
        }
        true
    });
    let bridge = Arc::new(Bridge {
        url,
        session: OnceCell::new(),
        next: AtomicU64::new(1),
        tools: list,
    });
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();
    let mut jobs = JoinSet::new();
    let mut eof = false;
    while !eof || !jobs.is_empty() {
        tokio::select! {
            line = lines.next_line(), if !eof => match line? {
                Some(s) => {
                    let bridge = bridge.clone();
                    jobs.spawn(async move {
                        match serde_json::from_str(&s) {
                            Ok(v) => bridge.handle(v).await,
                            Err(_) => Some(json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"Parse error"}}))
                        }
                    });
                }
                None => eof = true
            },
            job = jobs.join_next(), if !jobs.is_empty() => {
                if let Some(packet) = job.unwrap().map_err(io::Error::other)? {
                    stdout.write_all(format!("{packet}\n").as_bytes()).await?;
                    stdout.flush().await?;
                }
            }
        }
    }
    Ok(())
}
