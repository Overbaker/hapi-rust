use futures_util::{future::BoxFuture, FutureExt, SinkExt, StreamExt};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, VecDeque},
    io,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::{TcpListener, UnixListener, UnixStream},
    sync::{mpsc, oneshot},
    task::{JoinHandle, JoinSet},
    time::{timeout, Instant},
};
use tokio_tungstenite::{
    accept_hdr_async_with_config, client_async_with_config,
    tungstenite::{
        client::IntoClientRequest,
        handshake::server::{ErrorResponse, Request, Response},
        protocol::{frame::coding::CloseCode, CloseFrame, WebSocketConfig},
        Message,
    },
    WebSocketStream,
};
use uuid::Uuid;

pub trait Hooks: Send + Sync + 'static {
    fn before<'a>(
        &'a self,
        request: Value,
        connection: &'a str,
    ) -> BoxFuture<'a, Result<Value, String>>;
    fn after<'a>(
        &'a self,
        request: Value,
        response: Value,
        connection: &'a str,
    ) -> BoxFuture<'a, Result<Vec<Value>, String>>;
    fn control(&self, method: String, params: Value) -> BoxFuture<'_, Result<Value, String>>;
    fn disconnected(&self, _connection: &str) {}
}

fn config() -> Option<WebSocketConfig> {
    Some(
        WebSocketConfig::default()
            .max_message_size(Some(256 << 20))
            .max_frame_size(Some(256 << 20)),
    )
}
pub struct Gateway {
    pub endpoint: String,
    path: Option<PathBuf>,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}
impl Gateway {
    pub async fn start(
        path: Option<&Path>,
        upstream: PathBuf,
        token: String,
        hooks: Arc<dyn Hooks>,
    ) -> io::Result<Self> {
        let (stop, mut stopping) = oneshot::channel();
        let mut clients = JoinSet::new();
        macro_rules! serve {
            ($listener:expr, $auth:expr) => {{
                let listener = $listener;
                tokio::spawn(async move {
                    loop {
                        tokio::select! {
                            _ = &mut stopping => break,
                            accepted = listener.accept() => match accepted {
                                Ok((stream,_)) => { clients.spawn(connection(stream, upstream.clone(), $auth, hooks.clone())); }
                                Err(_) => break,
                            },
                            _ = clients.join_next(), if !clients.is_empty() => {}
                        }
                    }
                    clients.shutdown().await;
                })
            }};
        }
        let (endpoint, task) = if let Some(path) = path {
            let listener = UnixListener::bind(path)?;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
            (format!("unix://{}", path.display()), serve!(listener, None))
        } else {
            let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
            (
                format!("ws://{}", listener.local_addr()?),
                serve!(listener, Some(token.clone())),
            )
        };
        Ok(Self {
            endpoint,
            path: path.map(Path::to_owned),
            stop: Some(stop),
            task: Some(task),
        })
    }
    pub async fn close(mut self) {
        self.stop.take();
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}
impl Drop for Gateway {
    fn drop(&mut self) {
        self.stop.take();
        if let Some(path) = self.path.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}
struct Connection {
    hooks: Arc<dyn Hooks>,
    id: String,
}
impl Drop for Connection {
    fn drop(&mut self) {
        self.hooks.disconnected(&self.id);
    }
}
fn envelope(message: &Message) -> Result<Option<Value>, ()> {
    match message {
        Message::Text(_) | Message::Binary(_) => {
            let v: Value =
                serde_json::from_str(message.to_text().map_err(|_| ())?).map_err(|_| ())?;
            if !v.is_object()
                || v.get("method").is_some_and(|m| !m.is_string())
                || v.get("id")
                    .is_some_and(|id| !id.is_number() && !id.is_string())
            {
                return Err(());
            }
            Ok(Some(v))
        }
        _ => Ok(None),
    }
}
fn response(request: &Value, key: &str, value: Value) -> Value {
    let mut reply = json!({});
    if let Some(id) = request.get("id") {
        reply["id"] = id.clone()
    }
    reply[key] = value;
    reply
}
fn error(request: &Value, code: i32, message: String) -> Value {
    response(request, "error", json!({"code":code,"message":message}))
}
fn lifecycle(v: &Value) -> bool {
    matches!(
        v["method"].as_str(),
        Some(
            "thread/start"
                | "thread/resume"
                | "thread/fork"
                | "thread/archive"
                | "thread/revert"
                | "thread/rollback"
                | "thread/queue/delete"
        )
    ) && v.get("id").is_some()
}
fn close(code: CloseCode, reason: &'static str) -> Message {
    Message::Close(Some(CloseFrame {
        code,
        reason: reason.into(),
    }))
}
fn frame(v: &Value) -> Message {
    Message::text(v.to_string())
}
#[allow(clippy::result_large_err)]
fn authorize(
    request: &Request,
    response: Response,
    token: Option<&str>,
) -> Result<Response, ErrorResponse> {
    if let Some(token) = token {
        let expected = format!("Bearer {token}");
        let actual = request
            .headers()
            .get("Authorization")
            .map_or(&[][..], |v| v.as_bytes());
        if expected.len() != actual.len()
            || expected
                .bytes()
                .zip(actual)
                .fold(0, |a, (x, y)| a | (x ^ y))
                != 0
        {
            let mut r = ErrorResponse::new(None);
            *r.status_mut() = tokio_tungstenite::tungstenite::http::StatusCode::UNAUTHORIZED;
            return Err(r);
        }
    }
    Ok(response)
}
async fn connection<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(
    stream: S,
    path: PathBuf,
    token: Option<String>,
    hooks: Arc<dyn Hooks>,
) {
    #[allow(clippy::result_large_err)]
    let handshake = |r: &Request, s: Response| authorize(r, s, token.as_deref());
    let Ok(mut downstream) = accept_hdr_async_with_config(stream, handshake, config()).await else {
        return;
    };
    let connection = Connection {
        hooks,
        id: Uuid::new_v4().to_string(),
    };
    let ready = timeout(Duration::from_secs(10), async {
        let stream = UnixStream::connect(path).await?;
        let request = "ws://localhost/".into_client_request()?;
        client_async_with_config(request, stream, config()).await
    })
    .await;
    let Ok(Ok((upstream, _))) = ready else {
        let _ = downstream
            .send(close(CloseCode::Error, "Codex unavailable"))
            .await;
        return;
    };
    relay(downstream, upstream, &connection).await;
}

enum Destination {
    Up(Value),
    Down(Value),
}
async fn before(c: &Connection, v: Value) -> Destination {
    let method = v["method"].as_str().unwrap_or_default();
    if method.starts_with("hapi/") {
        return Destination::Down(
            match c.hooks.control(method.into(), v["params"].clone()).await {
                Ok(r) => response(&v, "result", r),
                Err(e) => error(&v, -32600, e),
            },
        );
    }
    let identity = response(&v, "result", Value::Null);
    match c.hooks.before(v, &c.id).await {
        Ok(r)
            if r["method"].as_str().is_none_or(str::is_empty)
                && (r.get("error").is_some() || r.get("result").is_some()) =>
        {
            Destination::Down(r)
        }
        Ok(r) => Destination::Up(r),
        Err(e) => Destination::Down(error(&identity, -32600, e)),
    }
}

async fn write_frames<S: AsyncRead + AsyncWrite + Unpin>(
    mut socket: futures_util::stream::SplitSink<WebSocketStream<S>, Message>,
    mut frames: mpsc::UnboundedReceiver<Message>,
    downstream: bool,
) -> bool {
    while let Some(frame) = frames.recv().await {
        let closing = frame.is_close();
        if socket.send(frame).await.is_err() || closing {
            break;
        }
    }
    downstream
}
async fn relay<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(
    downstream: WebSocketStream<S>,
    upstream: WebSocketStream<UnixStream>,
    c: &Connection,
) {
    let (down_sink, mut down_read) = downstream.split();
    let (up_sink, mut up_read) = upstream.split();
    let (down, down_frames) = mpsc::unbounded_channel();
    let (up, up_frames) = mpsc::unbounded_channel();
    let mut writers = JoinSet::new();
    writers.spawn(write_frames(down_sink, down_frames, true));
    writers.spawn(write_frames(up_sink, up_frames, false));
    let mut incoming = VecDeque::new();
    let mut outgoing: VecDeque<(Value, Message)> = VecDeque::new();
    let mut pending = HashMap::<String, Value>::new();
    let mut pre = None;
    let mut post = None;
    let mut detached = None;
    let mut upstream_closed = false;
    let deadline = || Instant::now() + Duration::from_secs(60);
    loop {
        if pre.is_none() {
            pre = incoming.pop_front().map(|v| before(c, v).boxed());
        }
        if post.is_none() {
            if let Some((v, raw)) = outgoing.pop_front() {
                let request = v
                    .get("id")
                    .filter(|_| v.get("method").is_none())
                    .and_then(|id| pending.remove(&hapi_wrapper_core::jsjson::stringify(id)));
                post = Some(
                    async move {
                        let mut frames = vec![raw];
                        if let Some(r) = request {
                            let identity = response(&v, "result", Value::Null);
                            match c.hooks.after(r, v, &c.id).await {
                                Ok(extra) => frames.extend(extra.iter().map(frame)),
                                Err(e) => return vec![frame(&error(&identity, -32603, e))],
                            }
                        }
                        frames
                    }
                    .boxed(),
                );
            }
        }
        if detached.is_some()
            && pre.is_none()
            && post.is_none()
            && (upstream_closed || pending.is_empty())
        {
            break;
        }
        tokio::select! {
            _ = async {tokio::time::sleep_until(detached.unwrap()).await}, if detached.is_some() => break,
            finished = writers.join_next(), if !writers.is_empty() => {
                if matches!(finished,Some(Ok(false))) {
                    upstream_closed=true;
                    let _=down.send(close(CloseCode::Error,"Codex disconnected"));
                }
                detached.get_or_insert_with(deadline);
            }
            result = async {pre.as_mut().unwrap().await}, if pre.is_some() => {
                pre = None;
                match result {
                    Destination::Up(v) if !upstream_closed => {
                        if lifecycle(&v) { pending.insert(hapi_wrapper_core::jsjson::stringify(&v["id"]), v.clone()); }
                        if up.send(frame(&v)).is_err() {
                            upstream_closed = true;
                            let _ = down.send(close(CloseCode::Error, "Codex unavailable"));
                            detached.get_or_insert_with(deadline);
                        }
                    }
                    Destination::Down(v) if detached.is_none() && down.send(frame(&v)).is_err() => {
                        detached = Some(deadline());
                    }
                    _ => {}
                }
            }
            frames = async {post.as_mut().unwrap().await}, if post.is_some() => {
                post = None;
                if detached.is_none() {
                    for frame in frames {
                        if down.send(frame).is_err() { detached = Some(deadline()); break; }
                    }
                }
            }
            packet = down_read.next(), if detached.is_none() => match packet {
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => detached = Some(deadline()),
                Some(Ok(msg)) => match envelope(&msg) {
                    Ok(Some(v)) if v["method"].as_str().is_some_and(|s| !s.is_empty()) => incoming.push_back(v),
                    Ok(Some(v)) => {
                        if up.send(frame(&v)).is_err() {
                            upstream_closed = true;
                            let _ = down.send(close(CloseCode::Error, "Codex unavailable"));
                            detached = Some(deadline());
                        }
                    }
                    Err(()) => {
                        let _ = down.send(close(CloseCode::Invalid, "Invalid JSON-RPC"));
                        detached = Some(deadline());
                    }
                    _ => {}
                }
            },
            packet = up_read.next(), if !upstream_closed => match packet {
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => {
                    upstream_closed = true;
                    let _ = down.send(close(CloseCode::Error, "Codex disconnected"));
                    detached.get_or_insert_with(deadline);
                }
                Some(Ok(msg)) => match envelope(&msg) {
                    Ok(Some(v)) => {
                        let raw = match msg {
                            Message::Text(_) => msg,
                            _ => frame(&v)
                        };
                        outgoing.push_back((v, raw));
                    }
                    Err(()) => {
                        let _ = down.send(close(CloseCode::Error, "Invalid Codex response"));
                        detached.get_or_insert_with(deadline);
                    }
                    _ => {}
                }
            }
        }
    }
    drop(up);
    drop(down);
    let drain_until = detached.unwrap_or_else(deadline);
    let _ = tokio::time::timeout_at(drain_until, async {
        while writers.join_next().await.is_some() {}
    })
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::{mpsc, Semaphore};
    struct TestHooks {
        seen: mpsc::UnboundedSender<String>,
        release: Semaphore,
        finished: Semaphore,
        detached: Semaphore,
    }
    impl Hooks for TestHooks {
        fn before<'a>(&'a self, v: Value, _: &'a str) -> BoxFuture<'a, Result<Value, String>> {
            async move {
                Ok(if v["method"] == "thread/queue/delete" {
                    json!({"id":v["id"],"result":{"deleted":true}})
                } else {
                    v
                })
            }
            .boxed()
        }
        fn after<'a>(
            &'a self,
            _: Value,
            _: Value,
            id: &'a str,
        ) -> BoxFuture<'a, Result<Vec<Value>, String>> {
            async move {
                self.seen.send(id.into()).unwrap();
                self.release.acquire().await.unwrap().forget();
                self.finished.add_permits(1);
                Ok(vec![json!({"method":"bound"})])
            }
            .boxed()
        }
        fn disconnected(&self, _: &str) {
            self.detached.add_permits(1);
        }
        fn control(&self, m: String, _: Value) -> BoxFuture<'_, Result<Value, String>> {
            async move {
                match m.as_str() {
                    "hapi/stopSession" => Ok(json!({"stopped":true})),
                    "hapi/attach" => Ok(json!({"threadId":"thread"})),
                    _ => Err("Unknown runtime control".into()),
                }
            }
            .boxed()
        }
    }
    async fn fixture() -> (
        tempfile::TempDir,
        Gateway,
        Arc<TestHooks>,
        mpsc::UnboundedReceiver<String>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let engine = dir.path().join("engine.sock");
        let listener = UnixListener::bind(&engine).unwrap();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut socket = tokio_tungstenite::accept_async_with_config(stream, config())
                        .await
                        .unwrap();
                    while let Some(Ok(Message::Text(data))) = socket.next().await {
                        let request: Value = serde_json::from_str(data.as_ref()).unwrap();
                        assert_ne!(request["method"], "thread/queue/delete");
                        if let Some(id) = request.get("id") {
                            let response = json!({"id":id,"result":request["params"]});
                            if socket.send(frame(&response)).await.is_err() {
                                break;
                            }
                        }
                    }
                });
            }
        });
        let (tx, rx) = mpsc::unbounded_channel();
        let hooks = Arc::new(TestHooks {
            seen: tx,
            release: Semaphore::new(0),
            finished: Semaphore::new(0),
            detached: Semaphore::new(0),
        });
        let gateway = Gateway::start(
            Some(&dir.path().join("clients.sock")),
            engine,
            "secret".into(),
            hooks.clone(),
        )
        .await
        .unwrap();
        (dir, gateway, hooks, rx)
    }
    async fn connect(dir: &Path) -> WebSocketStream<UnixStream> {
        client_async_with_config(
            "ws://localhost/",
            UnixStream::connect(dir.join("clients.sock")).await.unwrap(),
            config(),
        )
        .await
        .unwrap()
        .0
    }
    async fn receive(ws: &mut WebSocketStream<UnixStream>) -> Value {
        serde_json::from_slice(&ws.next().await.unwrap().unwrap().into_data()).unwrap()
    }
    #[tokio::test]
    async fn lifecycle_connections_and_detach() {
        let (dir, gateway, hooks, mut seen) = fixture().await;
        let mut first = connect(dir.path()).await;
        let mut second = connect(dir.path()).await;
        for (socket, param) in [(&mut first, "a"), (&mut second, "b")] {
            socket
                .send(frame(
                    &json!({"id":1,"method":"thread/start","params":param}),
                ))
                .await
                .unwrap();
        }
        assert_ne!(seen.recv().await.unwrap(), seen.recv().await.unwrap());
        assert!(timeout(Duration::from_millis(25), first.next())
            .await
            .is_err());
        first.close(None).await.unwrap();
        hooks.release.add_permits(2);
        assert_eq!(receive(&mut second).await, json!({"id":1,"result":"b"}));
        assert_eq!(receive(&mut second).await, json!({"method":"bound"}));
        hooks.finished.acquire_many(2).await.unwrap().forget();
        gateway.close().await;
        assert!(!dir.path().join("clients.sock").exists());
    }
    #[tokio::test]
    async fn abandons_unsettled_lifecycle_after_sixty_seconds() {
        let (dir, gateway, hooks, mut seen) = fixture().await;
        let mut ws = connect(dir.path()).await;
        ws.send(frame(&json!({"id":8,"method":"thread/resume"})))
            .await
            .unwrap();
        seen.recv().await.unwrap();
        ws.close(None).await.unwrap();
        drop(ws);
        tokio::time::pause();
        assert!(timeout(Duration::from_secs(59), hooks.detached.acquire())
            .await
            .is_err());
        timeout(Duration::from_secs(2), hooks.detached.acquire())
            .await
            .unwrap()
            .unwrap()
            .forget();
        assert_eq!(hooks.finished.available_permits(), 0);
        gateway.close().await;
    }
    #[tokio::test]
    async fn local_control_and_invalid_json() {
        let (dir, gateway, _, mut seen) = fixture().await;
        let mut ws = connect(dir.path()).await;
        for (method, result) in [
            ("thread/queue/delete", json!({"deleted":true})),
            ("hapi/stopSession", json!({"stopped":true})),
            ("hapi/attach", json!({"threadId":"thread"})),
        ] {
            ws.send(frame(&json!({"id":7,"method":method})))
                .await
                .unwrap();
            assert_eq!(receive(&mut ws).await, json!({"id":7,"result":result}));
        }
        ws.send(frame(&json!({"id":"bad-control","method":"hapi/unknown"})))
            .await
            .unwrap();
        assert_eq!(
            receive(&mut ws).await,
            json!({"id":"bad-control","error":{"code":-32600,"message":"Unknown runtime control"}})
        );
        assert!(seen.try_recv().is_err());
        ws.send(Message::text("invalid")).await.unwrap();
        assert!(
            matches!(ws.next().await,Some(Ok(Message::Close(Some(f)))) if f.code==CloseCode::Invalid)
        );
        gateway.close().await;
    }
    #[tokio::test]
    async fn large_history_round_trip() {
        let (dir, gateway, _, _) = fixture().await;
        let mut client = super::super::app_server_client::Client::connect(
            &dir.path().join("clients.sock"),
            None,
        )
        .await
        .unwrap();
        client.initialize().await.unwrap();
        for size in [65 << 20, 5] {
            let result = client
                .request_timeout(
                    "thread/read",
                    Value::String("x".repeat(size)),
                    Duration::from_secs(90),
                )
                .await
                .unwrap();
            assert_eq!(result.as_str().unwrap(), "x".repeat(size));
        }
        gateway.close().await;
    }
    #[tokio::test]
    async fn loopback_requires_authentication() {
        let (dir, _unix, hooks, _) = fixture().await;
        let gateway = Gateway::start(None, dir.path().join("engine.sock"), "secret".into(), hooks)
            .await
            .unwrap();
        let stream = tokio::net::TcpStream::connect(gateway.endpoint.trim_start_matches("ws://"))
            .await
            .unwrap();
        let err = client_async_with_config(gateway.endpoint.as_str(), stream, config())
            .await
            .unwrap_err();
        assert!(matches!(err,tokio_tungstenite::tungstenite::Error::Http(r) if r.status()==401));
        gateway.close().await;
    }
}
