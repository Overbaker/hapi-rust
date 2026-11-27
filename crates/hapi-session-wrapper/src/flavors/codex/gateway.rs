use super::transport;
use futures_util::{future::BoxFuture, FutureExt, SinkExt, StreamExt};
use hapi_wrapper_core::jsjson::parse as parse_json;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    io,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
#[cfg(all(test, unix))]
use tokio::net::UnixStream;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpListener,
    sync::{mpsc, oneshot},
    task::{JoinHandle, JoinSet},
    time::{timeout, Instant},
};
use tokio_tungstenite::{
    accept_hdr_async_with_config, client_async_with_config,
    tungstenite::{
        handshake::server::{ErrorResponse, Request, Response},
        protocol::{frame::coding::CloseCode, CloseFrame, WebSocketConfig},
        Message,
    },
    WebSocketStream,
};
use uuid::Uuid;
#[cfg(unix)]
use {std::os::unix::fs::PermissionsExt, tokio::net::UnixListener};

pub trait Hooks: Send + Sync + 'static {
    fn before<'a>(
        &'a self,
        request: Value,
        connection: &'a str,
    ) -> BoxFuture<'a, Result<Value, String>>;
    fn after<'a>(
        &'a self,
        request: Value,
        response: Arc<Value>,
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
fn retry_accept(error: &io::Error) -> bool {
    #[cfg(unix)]
    return !matches!(
        error.raw_os_error(),
        Some(libc::EBADF | libc::EINVAL | libc::ENOTSOCK | libc::EOPNOTSUPP)
    );
    #[cfg(windows)]
    return !matches!(error.raw_os_error(), Some(10022 | 10038 | 10045));
}
async fn accept_retry<T, F: std::future::Future<Output = io::Result<T>>>(
    mut accept: impl FnMut() -> F,
) -> io::Result<T> {
    loop {
        match accept().await {
            Err(e) if retry_accept(&e) => {
                eprintln!("Codex gateway accept failed: {e}; retrying");
                tokio::time::sleep(Duration::from_millis(75)).await;
            }
            result => return result,
        }
    }
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
        upstream: transport::Endpoint,
        token: String,
        hooks: Arc<dyn Hooks>,
    ) -> io::Result<Self> {
        let (stop, stopping) = oneshot::channel();
        let (endpoint, task) = if let Some(path) = path {
            #[cfg(windows)]
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("unsupported Unix socket path {}", path.display()),
            ));
            #[cfg(unix)]
            {
                let listener = UnixListener::bind(path)?;
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
                (
                    format!("unix://{}", path.display()),
                    tokio::spawn(async move {
                        serve(|| listener.accept(), stopping, upstream, None, hooks).await
                    }),
                )
            }
        } else {
            let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
            (
                format!("ws://{}", listener.local_addr()?),
                tokio::spawn(async move {
                    serve(|| listener.accept(), stopping, upstream, Some(token), hooks).await
                }),
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
async fn serve<S, A, F>(
    mut accept: impl FnMut() -> F,
    mut stopping: oneshot::Receiver<()>,
    upstream: transport::Endpoint,
    auth: Option<String>,
    hooks: Arc<dyn Hooks>,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    F: std::future::Future<Output = io::Result<(S, A)>>,
{
    let mut clients = JoinSet::new();
    'listen: loop {
        let accepting = accept_retry(&mut accept);
        tokio::pin!(accepting);
        loop {
            tokio::select! {
                _=&mut stopping=>break 'listen,
                result=&mut accepting=>match result{
                    Ok((stream,_))=>{clients.spawn(connection(stream,upstream.clone(),auth.clone(),hooks.clone()));break;}
                    Err(e)=>{eprintln!("Codex gateway listener stopped: {e}");break 'listen;}
                },
                _=clients.join_next(),if !clients.is_empty()=>{}
            }
        }
    }
    clients.shutdown().await;
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
            let text = message.to_text().map_err(|_| ())?;
            let v: Value = parse_json(text.as_bytes()).map_err(|_| ())?;
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
    path: transport::Endpoint,
    token: Option<String>,
    hooks: Arc<dyn Hooks>,
) {
    #[allow(clippy::result_large_err)]
    let handshake = |r: &Request, s: Response| authorize(r, s, token.as_deref());
    let Ok(Ok(mut downstream)) = timeout(
        Duration::from_secs(10),
        accept_hdr_async_with_config(stream, handshake, config()),
    )
    .await
    else {
        return;
    };
    let connection = Connection {
        hooks,
        id: Uuid::new_v4().to_string(),
    };
    let ready = timeout(Duration::from_secs(10), async {
        let stream = transport::stream(&path).await?;
        let request = transport::request(&path, None)?;
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

enum Write {
    Frame(Message),
    Flush,
}
struct Writer(mpsc::Sender<Write>);
impl Writer {
    async fn send(&self, frame: Message) -> Result<(), mpsc::error::SendError<Write>> {
        self.0.send(Write::Frame(frame)).await
    }
    async fn flush(&self) {
        let _ = self.0.send(Write::Flush).await;
    }
}
async fn write_frames<S: AsyncRead + AsyncWrite + Unpin>(
    mut socket: futures_util::stream::SplitSink<WebSocketStream<S>, Message>,
    mut frames: mpsc::Receiver<Write>,
    downstream: bool,
) -> bool {
    while let Some(write) = frames.recv().await {
        let closing = matches!(&write, Write::Frame(Message::Close(_)));
        let io = async {
            match write {
                Write::Frame(msg) => socket.send(msg).await,
                Write::Flush => socket.flush().await,
            }
        };
        if !matches!(timeout(Duration::from_secs(30), io).await, Ok(Ok(()))) || closing {
            break;
        }
    }
    downstream
}
async fn relay<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(
    downstream: WebSocketStream<S>,
    upstream: WebSocketStream<transport::Stream>,
    c: &Connection,
) {
    let (down_sink, mut down_read) = downstream.split();
    let (up_sink, mut up_read) = upstream.split();
    let (down, down_frames) = mpsc::channel(1);
    let (up, up_frames) = mpsc::channel(1);
    let (down, up) = (Writer(down), Writer(up));
    let mut writers = JoinSet::new();
    writers.spawn(write_frames(down_sink, down_frames, true));
    writers.spawn(write_frames(up_sink, up_frames, false));
    let mut incoming = None;
    let mut pending = HashMap::<String, (Value, usize)>::new();
    let mut pending_bytes = 0usize;
    let mut pre = None;
    let mut post = None;
    let mut detached = None;
    let mut upstream_closed = false;
    let deadline = || Instant::now() + Duration::from_secs(60);
    loop {
        if pre.is_none() {
            pre = incoming.take().map(|v| before(c, v).boxed());
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
                    let _=down.send(close(CloseCode::Error,"Codex disconnected")).await;
                }
                detached.get_or_insert_with(deadline);
            }
            result = async {pre.as_mut().unwrap().await}, if pre.is_some() => {
                pre = None;
                match result {
                    Destination::Up(v) if !upstream_closed => {
                        let msg=frame(&v);
                        if lifecycle(&v){
                            let key=hapi_wrapper_core::jsjson::stringify(&v["id"]);
                            if pending.len()>=128 || pending_bytes+msg.len()>256<<20 || pending.contains_key(&key){
                                let _=down.send(close(CloseCode::Policy,"Too many outstanding lifecycle requests")).await;
                                break;
                            }
                            pending_bytes+=msg.len();
                            pending.insert(key,(v,msg.len()));
                        }else{drop(v);}
                        if up.send(msg).await.is_err() {
                            upstream_closed = true;
                            let _ = down.send(close(CloseCode::Error, "Codex unavailable")).await;
                            detached.get_or_insert_with(deadline);
                        }
                    }
                    Destination::Down(v) if detached.is_none() => {
                        let msg=frame(&v);drop(v);
                        if down.send(msg).await.is_err(){detached=Some(deadline());}
                    }
                    _ => {}
                }
            }
            frames = async {post.as_mut().unwrap().await}, if post.is_some() => {
                post = None;
                if detached.is_none() {
                    for frame in frames {
                        if down.send(frame).await.is_err() { detached = Some(deadline()); break; }
                    }
                }
            }
            packet = down_read.next(), if detached.is_none() && incoming.is_none() => match packet {
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => detached = Some(deadline()),
                Some(Ok(Message::Ping(_)))=>down.flush().await,
                Some(Ok(msg)) => match envelope(&msg) {
                    Ok(Some(v)) if v["method"].as_str().is_some_and(|s| !s.is_empty()) => incoming=Some(v),
                    Ok(Some(v)) => {
                        drop(v);
                        if up.send(Message::Text(msg.into_text().unwrap())).await.is_err() {
                            upstream_closed = true;
                            let _ = down.send(close(CloseCode::Error, "Codex unavailable")).await;
                            detached = Some(deadline());
                        }
                    }
                    Err(()) => {
                        let _ = down.send(close(CloseCode::Invalid, "Invalid JSON-RPC")).await;
                        detached = Some(deadline());
                    }
                    _ => {}
                }
            },
            packet = up_read.next(), if !upstream_closed && post.is_none() => match packet {
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => {
                    upstream_closed = true;
                    let _ = down.send(close(CloseCode::Error, "Codex disconnected")).await;
                    detached.get_or_insert_with(deadline);
                }
                Some(Ok(Message::Ping(_)))=>up.flush().await,
                Some(Ok(msg)) => match envelope(&msg) {
                    Ok(Some(v)) => {
                        let key=v.get("id").filter(|_|v.get("method").is_none()).map(hapi_wrapper_core::jsjson::stringify);
                        if let Some((request,size))=key.and_then(|k|pending.remove(&k)){
                            pending_bytes-=size;
                            drop(msg);
                            let response=Arc::new(v);
                            post=Some(async move{
                                match c.hooks.after(request,response.clone(),&c.id).await{
                                    Ok(extra)=>{let mut frames=vec![frame(&response)];frames.extend(extra.iter().map(frame));frames}
                                    Err(e)=>vec![frame(&error(&response,-32603,e))]
                                }
                            }.boxed());
                        }else if detached.is_none(){
                            drop(v);
                            let raw=Message::Text(msg.into_text().unwrap());
                            if down.send(raw).await.is_err(){detached=Some(deadline());}
                        }
                    }
                    Err(()) => {
                        upstream_closed=true;
                        let _ = down.send(close(CloseCode::Error, "Invalid Codex response")).await;
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

#[cfg(all(test, unix))]
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
            _: Arc<Value>,
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
    #[tokio::test]
    async fn transient_accept_keeps_existing_connections() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let (dir, _original, hooks, _seen) = fixture().await;
        let path = dir.path().join("retry.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let engine = dir.path().join("engine.sock");
        let (stop, shutdown) = oneshot::channel();
        let task = tokio::spawn(async move {
            let calls = AtomicUsize::new(0);
            serve(
                || async {
                    if calls.fetch_add(1, Ordering::Relaxed) == 1 {
                        return Err(io::Error::from_raw_os_error(libc::EMFILE));
                    }
                    listener.accept().await
                },
                shutdown,
                engine,
                None,
                hooks,
            )
            .await
        });
        let connect = || async {
            client_async_with_config(
                "ws://localhost/",
                UnixStream::connect(&path).await.unwrap(),
                config(),
            )
            .await
            .unwrap()
            .0
        };
        let mut first = connect().await;
        let mut second = connect().await;
        for socket in [&mut first, &mut second] {
            socket
                .send(frame(&json!({"id":7,"method":"hapi/stopSession"})))
                .await
                .unwrap();
            assert_eq!(receive(socket).await["result"]["stopped"], true);
        }
        drop(stop);
        task.await.unwrap();
    }
    async fn pair() -> (WebSocketStream<UnixStream>, WebSocketStream<UnixStream>) {
        use tokio_tungstenite::tungstenite::protocol::Role;
        let (a, b) = UnixStream::pair().unwrap();
        tokio::join!(
            WebSocketStream::from_raw_socket(a, Role::Client, config()),
            WebSocketStream::from_raw_socket(b, Role::Server, config())
        )
    }
    async fn relay_pair() -> (
        WebSocketStream<UnixStream>,
        WebSocketStream<UnixStream>,
        Arc<TestHooks>,
        mpsc::UnboundedReceiver<String>,
    ) {
        let (client, down) = pair().await;
        let (up, engine) = pair().await;
        let (seen, rx) = mpsc::unbounded_channel();
        let hooks = Arc::new(TestHooks {
            seen,
            release: Semaphore::new(0),
            finished: Semaphore::new(0),
            detached: Semaphore::new(0),
        });
        let guard = Connection {
            hooks: hooks.clone(),
            id: "test".into(),
        };
        tokio::spawn(async move { relay(down, up, &guard).await });
        (client, engine, hooks, rx)
    }
    #[tokio::test]
    async fn idle_ping_in_both_directions_and_invalid_upstream() {
        let (mut client, mut engine, hooks, _) = relay_pair().await;
        for ws in [&mut client, &mut engine] {
            ws.send(Message::Ping(vec![42].into())).await.unwrap();
            assert!(
                matches!(timeout(Duration::from_secs(2),ws.next()).await.unwrap(),Some(Ok(Message::Pong(v))) if v.as_ref()==[42])
            );
        }
        client
            .send(frame(&json!({"id":1,"method":"thread/start"})))
            .await
            .unwrap();
        assert_eq!(receive(&mut engine).await["id"], 1);
        engine.send(Message::text("broken")).await.unwrap();
        assert!(
            matches!(client.next().await,Some(Ok(Message::Close(Some(f)))) if f.code==CloseCode::Error)
        );
        timeout(Duration::from_secs(2), hooks.detached.acquire())
            .await
            .unwrap()
            .unwrap()
            .forget();
    }
    #[tokio::test]
    async fn lifecycle_barrier_backpressures_the_upstream_socket() {
        let (mut client, mut engine, hooks, mut seen) = relay_pair().await;
        client
            .send(frame(&json!({"id":1,"method":"thread/start"})))
            .await
            .unwrap();
        receive(&mut engine).await;
        engine
            .send(frame(&json!({"id":1,"result":{}})))
            .await
            .unwrap();
        seen.recv().await.unwrap();
        let mut flood = tokio::spawn(async move {
            let data = frame(&json!({"method":"notice","params":"x".repeat(128<<10)}));
            for _ in 0..40 {
                engine.send(data.clone()).await.unwrap();
            }
            engine
        });
        assert!(timeout(Duration::from_millis(150), &mut flood)
            .await
            .is_err());
        hooks.release.add_permits(1);
        assert_eq!(receive(&mut client).await["id"], 1);
        receive(&mut client).await;
        for _ in 0..40 {
            assert_eq!(receive(&mut client).await["method"], "notice");
        }
        flood.await.unwrap();
    }
    #[tokio::test]
    async fn stalled_downstream_backpressures_engine_without_blocking_a_sibling() {
        let (mut slow, mut engine, _guard, _seen) = relay_pair().await;
        let mut flood = tokio::spawn(async move {
            let data = frame(&json!({"method":"notice","params":"x".repeat(256<<10)}));
            for _ in 0..64 {
                engine.send(data.clone()).await.unwrap();
            }
            engine
        });
        assert!(timeout(Duration::from_millis(200), &mut flood)
            .await
            .is_err());
        let (mut sibling, _engine, _guard, _seen) = relay_pair().await;
        sibling
            .send(frame(&json!({"id":1,"method":"hapi/attach"})))
            .await
            .unwrap();
        assert_eq!(
            timeout(Duration::from_secs(2), receive(&mut sibling))
                .await
                .unwrap()["result"]["threadId"],
            "thread"
        );
        for _ in 0..64 {
            assert!(slow.next().await.unwrap().unwrap().is_text());
        }
        flood.await.unwrap();
    }
}
