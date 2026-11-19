use crate::loopback::{reply, Listener, Reply};
use futures_util::{future::BoxFuture, stream};
use http_body_util::{BodyExt, Limited, StreamBody};
use hyper::{
    body::{Frame, Incoming},
    Method, Request, Response,
};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    convert::Infallible,
    io,
    sync::{Arc, Mutex},
};
use tokio::sync::watch;
use uuid::Uuid;

pub const VERSIONS: &[&str] = &[
    "2025-11-25",
    "2025-06-18",
    "2025-03-26",
    "2024-11-05",
    "2024-10-07",
];
pub trait Tools: Send + Sync + 'static {
    fn list(&self) -> Value;
    fn call(&self, name: String, args: Value) -> BoxFuture<'_, Value>;
}
type Sessions = Arc<Mutex<HashMap<String, watch::Sender<()>>>>;
const HEADER: &str = "mcp-session-id";

pub async fn serve(tools: Arc<impl Tools>) -> io::Result<Listener> {
    let sessions = Sessions::default();
    Listener::bind(move |req, port, stop| handle(req, port, stop, sessions.clone(), tools.clone()))
        .await
}
fn error(status: u16, code: i32, message: &str) -> Reply {
    reply(
        status,
        "application/json",
        json!({"jsonrpc":"2.0","error":{"code":code,"message":message},"id":null}).to_string(),
    )
}

fn loopback_origin(origin: &str) -> bool {
    let Some((scheme, host)) = origin.split_once("://") else {
        return false;
    };
    if !matches!(scheme, "http" | "https") || host.contains(['@', '/', '?', '#']) {
        return false;
    }
    let Ok(authority) = host.parse::<hyper::http::uri::Authority>() else {
        return false;
    };
    let hostname = authority.host();
    let suffix = &host[hostname.len()..];
    if !suffix.is_empty() && (!suffix.starts_with(':') || suffix[1..].parse::<u16>().is_err()) {
        return false;
    }
    hostname.eq_ignore_ascii_case("localhost")
        || hostname
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

async fn handle(
    req: Request<Incoming>,
    port: u16,
    mut stop: watch::Receiver<bool>,
    sessions: Sessions,
    tools: Arc<impl Tools>,
) -> Reply {
    let mut hosts = req.headers().get_all("host").iter();
    let allowed_host = hosts.next().and_then(|h| h.to_str().ok()).is_some_and(|h| {
        h == format!("127.0.0.1:{port}") || h.eq_ignore_ascii_case(&format!("localhost:{port}"))
    });
    let mut origins = req.headers().get_all("origin").iter();
    if !allowed_host
        || hosts.next().is_some()
        || origins
            .next()
            .is_some_and(|h| !h.to_str().is_ok_and(loopback_origin))
        || origins.next().is_some()
    {
        return error(403, -32000, "Forbidden: unexpected Host or Origin");
    }
    let sid = req
        .headers()
        .get(HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let session = sid
        .as_ref()
        .and_then(|s| sessions.lock().unwrap().get(s).map(|tx| tx.subscribe()));
    if sid.is_some() && session.is_none() {
        return reply(404, "text/plain", "");
    }
    if session.is_some()
        && matches!(*req.method(), Method::GET | Method::POST | Method::DELETE)
        && req
            .headers()
            .get_all("mcp-protocol-version")
            .iter()
            .any(|h| !h.to_str().is_ok_and(|s| VERSIONS.contains(&s)))
    {
        return error(400, -32000, "Bad Request: Unsupported protocol version");
    }
    match *req.method() {
        Method::DELETE if session.is_some() => {
            sessions.lock().unwrap().remove(sid.as_ref().unwrap());
            return reply(200, "text/plain", "");
        }
        Method::GET if session.is_some() => {
            let mut rx = session.unwrap();
            let body = stream::once(async move {
                tokio::select! { _ = rx.changed() => {}, _ = stop.wait_for(|v| *v) => {} }
                Ok::<_, Infallible>(Frame::data(bytes::Bytes::new()))
            });
            return Response::builder()
                .header("content-type", "text/event-stream")
                .header(HEADER, sid.unwrap())
                .body(StreamBody::new(body).boxed())
                .unwrap();
        }
        Method::POST => {}
        Method::GET | Method::DELETE => {
            return error(
                400,
                -32000,
                "Bad Request: Mcp-Session-Id header is required",
            )
        }
        _ => {
            let mut res = error(405, -32000, "Method not allowed.");
            res.headers_mut()
                .insert("allow", "GET, POST, DELETE".parse().unwrap());
            return res;
        }
    }
    if !req
        .headers()
        .get("content-type")
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.split(';').next())
        .is_some_and(|s| s.trim().eq_ignore_ascii_case("application/json"))
    {
        return error(
            415,
            -32000,
            "Unsupported Media Type: Content-Type must be application/json",
        );
    }
    let accept = req
        .headers()
        .get("accept")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("*/*");
    let allows = |mime: &str| {
        accept.split(',').any(|item| {
            let mut parts = item.trim().split(';');
            let kind = parts.next().unwrap_or("").trim();
            (kind == mime
                || kind == "*/*"
                || kind == format!("{}/*", mime.split('/').next().unwrap()))
                && parts
                    .filter_map(|p| p.trim().strip_prefix("q="))
                    .all(|q| q.parse::<f32>().unwrap_or(0.) > 0.)
        })
    };
    let use_json = allows("application/json");
    if !use_json && !allows("text/event-stream") {
        return error(
            406,
            -32000,
            "Not Acceptable: Client must accept application/json or text/event-stream",
        );
    }
    let Ok(body) = Limited::new(req.into_body(), 4 * 1024 * 1024)
        .collect()
        .await
    else {
        return error(413, -32000, "Request body too large");
    };
    let Ok(v) = serde_json::from_slice::<Value>(&body.to_bytes()) else {
        return reply(
            400,
            "application/json",
            json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"Parse error"}})
                .to_string(),
        );
    };
    let method = v["method"].as_str().unwrap_or("");
    if v["jsonrpc"] != "2.0" || method.is_empty() {
        return error(400, -32600, "Invalid request");
    }
    if method != "initialize" && session.is_none() {
        return error(
            400,
            -32000,
            "Bad Request: Mcp-Session-Id header is required",
        );
    }
    let id = &v["id"];
    if id.is_null() {
        return reply(202, "text/plain", "");
    }
    let mut response_sid = sid;
    let result = match method {
        "initialize" if session.is_none() => {
            let sid = Uuid::new_v4().to_string();
            sessions
                .lock()
                .unwrap()
                .insert(sid.clone(), watch::channel(()).0);
            response_sid = Some(sid);
            let version = v["params"]["protocolVersion"]
                .as_str()
                .filter(|v| VERSIONS.contains(v))
                .unwrap_or(VERSIONS[0]);
            Ok(
                json!({"protocolVersion":version,"serverInfo":{"name":"HAPI MCP","version":"1.0.0"},"capabilities":{"tools":{"listChanged":true}}}),
            )
        }
        "initialize" => return error(400, -32600, "Invalid Request: Server already initialized"),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(tools.list()),
        "tools/call" => {
            let name = v["params"]["name"].as_str().unwrap_or("");
            if tools.list()["tools"]
                .as_array()
                .is_some_and(|items| items.iter().any(|t| t["name"] == name))
            {
                Ok(tools
                    .call(
                        name.into(),
                        v["params"].get("arguments").cloned().unwrap_or(json!({})),
                    )
                    .await)
            } else {
                Ok(super::tools::result(Err(format!(
                    "MCP error -32602: Tool {name} not found"
                ))))
            }
        }
        _ => Err(json!({"code":-32601,"message":"Method not found"})),
    };
    let packet = match result {
        Ok(r) => json!({"jsonrpc":"2.0","id":id,"result":r}),
        Err(e) => json!({"jsonrpc":"2.0","id":id,"error":e}),
    };
    let mut res = if use_json {
        reply(200, "application/json", packet.to_string())
    } else {
        reply(
            200,
            "text/event-stream",
            format!("event: message\ndata: {packet}\n\n"),
        )
    };
    if let Some(s) = response_sid {
        res.headers_mut().insert(HEADER, s.parse().unwrap());
    }
    res
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use http_body_util::Full;
    use hyper_util::{client::legacy::Client, rt::TokioExecutor};
    #[test]
    fn sdk_contract() {
        let v: Value =
            serde_json::from_str(include_str!("../../golden/mcp_versions.json")).unwrap();
        assert_eq!(json!(VERSIONS), v["supported"]);
        assert_eq!(VERSIONS[0], v["latest"]);
        let status = std::process::Command::new("bun")
            .current_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/../../cli"))
            .args([
                "--tsconfig-override",
                "tsconfig.json",
                "../crates/hapi-wrapper-core/golden/generate.ts",
                "--check",
            ])
            .status()
            .unwrap();
        assert!(status.success());
    }
    struct Echo;
    impl Tools for Echo {
        fn list(&self) -> Value {
            json!({"tools":[{"name":"echo"}]})
        }
        fn call(&self, _: String, args: Value) -> BoxFuture<'_, Value> {
            Box::pin(async { args })
        }
    }
    #[tokio::test]
    async fn transport() {
        let server = serve(Arc::new(Echo)).await.unwrap();
        let client = Client::builder(TokioExecutor::new()).build_http::<Full<Bytes>>();
        let request = |method: Method, sid: &str, accept: &str, payload: Value| {
            let mut req = Request::builder()
                .uri(format!("http://127.0.0.1:{}/mcp", server.port))
                .method(method)
                .header("accept", accept)
                .header("content-type", "application/json");
            if !sid.is_empty() {
                req = req.header(HEADER, sid)
            }
            client.request(
                req.body(Full::new(Bytes::from(payload.to_string())))
                    .unwrap(),
            )
        };
        for version in VERSIONS.iter().copied().chain(["unsupported"]) {
            let res=request(Method::POST,"","application/json",json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":version}})).await.unwrap();
            let sid = res.headers()[HEADER].to_str().unwrap().to_owned();
            let result: Value =
                serde_json::from_slice(&res.collect().await.unwrap().to_bytes()).unwrap();
            assert_eq!(
                result["result"]["protocolVersion"],
                if version == "unsupported" {
                    VERSIONS[0]
                } else {
                    version
                }
            );
            let live = request(Method::GET, &sid, "text/event-stream", Value::Null)
                .await
                .unwrap();
            for (method, expected) in [("ping", "{}"), ("tools/list", "echo"), ("tools/call", "hi")]
            {
                let res=request(Method::POST,&sid,"text/event-stream",json!({"jsonrpc":"2.0","id":2,"method":method,"params":{"name":"echo","arguments":{"hi":true}}})).await.unwrap();
                assert_eq!(res.headers()["content-type"], "text/event-stream");
                let s =
                    String::from_utf8(res.collect().await.unwrap().to_bytes().to_vec()).unwrap();
                assert!(s.starts_with("event: message\ndata: ") && s.contains(expected));
            }
            assert_eq!(
                request(
                    Method::POST,
                    &sid,
                    "*/*",
                    json!({"jsonrpc":"2.0","method":"notifications/initialized"})
                )
                .await
                .unwrap()
                .status(),
                202
            );
            assert_eq!(
                request(Method::DELETE, &sid, "*/*", Value::Null)
                    .await
                    .unwrap()
                    .status(),
                200
            );
            live.collect().await.unwrap();
            assert_eq!(
                request(Method::GET, &sid, "*/*", Value::Null)
                    .await
                    .unwrap()
                    .status(),
                404
            );
        }
    }

    #[tokio::test]
    async fn request_guards_and_shutdown() {
        let server = serve(Arc::new(Echo)).await.unwrap();
        let port = server.port;
        let client = Client::builder(TokioExecutor::new()).build_http::<Full<Bytes>>();
        let request = |method: Method, headers: Vec<(&str, &str)>, method_name: &str| {
            let mut req = Request::builder()
                .method(method)
                .uri(format!("http://127.0.0.1:{port}/"));
            for (name, value) in headers {
                req = req.header(name, value);
            }
            client.request(
                req.body(Full::new(Bytes::from(
                    json!({"jsonrpc":"2.0","id":1,"method":method_name}).to_string(),
                )))
                .unwrap(),
            )
        };
        let localhost = format!("localhost:{port}");
        for headers in [
            vec![("host", "example.invalid")],
            vec![("host", "localhost:0")],
            vec![("origin", "null")],
            vec![("origin", "https://example.invalid")],
            vec![
                ("origin", "http://localhost"),
                ("origin", "https://example.invalid"),
            ],
        ] {
            let res = request(Method::POST, headers, "initialize").await.unwrap();
            assert_eq!(res.status(), 403);
            assert!(res.headers().get(HEADER).is_none());
        }
        let json = ("content-type", "application/json; charset=utf-8");
        let init = request(
            Method::POST,
            vec![
                json,
                ("host", &localhost),
                ("origin", "http://127.0.0.1:3000"),
            ],
            "initialize",
        )
        .await
        .unwrap();
        assert_eq!(init.status(), 200);
        let id = init.headers()[HEADER].to_str().unwrap().to_owned();
        let session = (HEADER, id.as_str());
        let bad_version = ("mcp-protocol-version", "unsupported");
        for (method, headers, rpc, status, code) in [
            (
                Method::POST,
                vec![json, bad_version, session],
                "ping",
                400,
                -32000,
            ),
            (Method::GET, vec![bad_version, session], "", 400, -32000),
            (Method::DELETE, vec![bad_version, session], "", 400, -32000),
            (Method::POST, vec![json, session], "initialize", 400, -32600),
            (Method::POST, vec![json], "ping", 400, -32000),
            (
                Method::POST,
                vec![json, ("accept", "text/plain")],
                "initialize",
                406,
                -32000,
            ),
            (
                Method::POST,
                vec![("content-type", "text/plain")],
                "initialize",
                415,
                -32000,
            ),
            (Method::PUT, vec![], "", 405, -32000),
        ] {
            let res = request(method, headers, rpc).await.unwrap();
            assert_eq!(res.status(), status);
            assert_eq!(res.headers()["content-type"], "application/json");
            if status == 405 {
                assert_eq!(res.headers()["allow"], "GET, POST, DELETE");
            }
            let msg: Value =
                serde_json::from_slice(&res.collect().await.unwrap().to_bytes()).unwrap();
            assert_eq!(msg["jsonrpc"], "2.0");
            assert_eq!(msg.get("id"), Some(&Value::Null));
            assert_eq!(msg["error"]["code"], code);
            assert!(msg["error"]["message"].is_string());
        }
        let stream = request(
            Method::GET,
            vec![session, ("mcp-protocol-version", VERSIONS[0])],
            "",
        )
        .await
        .unwrap();
        assert_eq!(stream.status(), 200);
        tokio::time::timeout(std::time::Duration::from_millis(500), async {
            server.shutdown().await;
            assert!(stream.collect().await.unwrap().to_bytes().is_empty());
        })
        .await
        .unwrap();
    }
}
