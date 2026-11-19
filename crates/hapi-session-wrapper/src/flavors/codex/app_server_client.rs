use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    fmt,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::{
    net::UnixStream,
    sync::{mpsc, oneshot},
    task::JoinHandle,
    time::timeout,
};
use tokio_tungstenite::{
    client_async_with_config,
    tungstenite::{
        client::IntoClientRequest,
        protocol::{frame::coding::CloseCode, CloseFrame, WebSocketConfig},
        Message,
    },
    WebSocketStream,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    Indeterminate(String),
    Rejected(String),
    NotReady(String),
}
impl Error {
    fn transport(e: impl fmt::Display) -> Self {
        Self::Indeterminate(e.to_string())
    }
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Indeterminate(s) | Self::Rejected(s) | Self::NotReady(s) => f.write_str(s),
        }
    }
}
impl std::error::Error for Error {}
type Result<T> = std::result::Result<T, Error>;
type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>>;
struct PendingId<'a>(&'a Pending, u64);
impl Drop for PendingId<'_> {
    fn drop(&mut self) {
        self.0.lock().unwrap().remove(&self.1);
    }
}

#[derive(Debug)]
struct Outbound {
    value: Value,
    flushed: Option<oneshot::Sender<()>>,
}
impl Outbound {
    fn message(value: Value) -> Self {
        Self {
            value,
            flushed: None,
        }
    }
}
#[derive(Debug)]
pub struct ServerRequest {
    pub id: Value,
    pub method: String,
    pub params: Value,
    out: mpsc::UnboundedSender<Outbound>,
}
impl ServerRequest {
    pub fn respond(self, result: Value) -> Result<()> {
        self.out
            .send(Outbound::message(json!({"id":self.id,"result":result})))
            .map_err(|_| Error::transport("Shared Codex connection closed"))
    }
}
#[derive(Debug)]
pub enum Event {
    Request(ServerRequest),
    Notification { method: String, params: Value },
    Disconnected(Error),
}

pub struct Client {
    initialized: bool,
    closed: AtomicBool,
    path: PathBuf,
    token: Option<String>,
    next: AtomicU64,
    abandon: mpsc::UnboundedSender<Error>,
    out: mpsc::UnboundedSender<Outbound>,
    pending: Pending,
    pub events: mpsc::UnboundedReceiver<Event>,
    task: JoinHandle<()>,
}
impl Drop for Client {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Client {
    pub async fn connect(path: &Path, token: Option<&str>) -> Result<Self> {
        let mut request = "ws://localhost/"
            .into_client_request()
            .map_err(Error::transport)?;
        if let Some(token) = token.filter(|v| !v.is_empty()) {
            request.headers_mut().insert(
                "Authorization",
                format!("Bearer {token}")
                    .parse()
                    .map_err(Error::transport)?,
            );
        }
        let config = WebSocketConfig::default()
            .max_message_size(Some(256 << 20))
            .max_frame_size(Some(256 << 20));
        let (ws, _) = timeout(Duration::from_secs(10), async {
            let stream = UnixStream::connect(path).await.map_err(Error::transport)?;
            client_async_with_config(request, stream, Some(config))
                .await
                .map_err(Error::transport)
        })
        .await
        .map_err(Error::transport)??;
        let (out, rx) = mpsc::unbounded_channel();
        let (tx, events) = mpsc::unbounded_channel();
        let pending = Pending::default();
        let (abandon, abandoned) = mpsc::unbounded_channel();
        let task = tokio::spawn(transport(
            ws,
            rx,
            out.clone(),
            tx,
            pending.clone(),
            abandoned,
        ));
        Ok(Self {
            initialized: false,
            closed: false.into(),
            path: path.into(),
            token: token.map(str::to_owned),
            next: AtomicU64::new(1),
            abandon,
            out,
            pending,
            events,
            task,
        })
    }
    pub(super) fn disconnect(&self) {
        self.closed.store(true, Ordering::Release);
        self.task.abort();
        for (_, tx) in self.pending.lock().unwrap().drain() {
            let _ = tx.send(Err(Error::transport("Shared Codex connection closed")));
        }
    }
    pub async fn reconnect(&mut self) -> Result<()> {
        let new = Self::connect(&self.path, self.token.as_deref()).await?;
        new.next
            .store(self.next.load(Ordering::Relaxed), Ordering::Relaxed);
        *self = new;
        self.initialize().await
    }
    pub async fn initialize(&mut self) -> Result<()> {
        self.request("initialize",json!({"clientInfo":{"name":"hapi","title":"HAPI","version":"1"},"capabilities":{"experimentalApi":true}})).await?;
        self.send(json!({"method":"initialized"}))?;
        self.initialized = true;
        Ok(())
    }
    fn send(&self, value: Value) -> Result<()> {
        self.out
            .send(Outbound::message(value))
            .map_err(|_| Error::transport("Shared Codex connection closed"))
    }
    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let seconds = match method {
            "initialize" | "model/list" => 30,
            _ => 20,
        };
        self.request_timeout(method, params, Duration::from_secs(seconds))
            .await
    }
    pub(super) async fn request_timeout(
        &self,
        method: &str,
        params: Value,
        duration: Duration,
    ) -> Result<Value> {
        if self.out.is_closed() {
            return Err(Error::NotReady(
                "Shared Codex client disconnected; reconnect and initialize before sending".into(),
            ));
        }
        if method != "initialize" && !self.initialized {
            return Err(Error::NotReady(
                "Shared Codex client is not initialized".into(),
            ));
        }
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock().unwrap();
            if self.closed.load(Ordering::Acquire) {
                return Err(Error::NotReady(
                    "Shared Codex client disconnected; reconnect and initialize before sending"
                        .into(),
                ));
            }
            pending.insert(id, tx);
        }
        let _guard = PendingId(&self.pending, id);
        let (flushed, mut dispatch) = oneshot::channel();
        self.out
            .send(Outbound {
                value: json!({"id":id,"method":method,"params":params}),
                flushed: Some(flushed),
            })
            .map_err(|_| Error::transport("Shared Codex connection closed"))?;
        match timeout(duration, rx).await {
            Ok(reply) => reply.map_err(|_| Error::transport("Shared Codex connection closed"))?,
            Err(_) => {
                let error = Error::transport(format!(
                    "Codex app-server request '{method}' timed out after {}ms",
                    duration.as_millis()
                ));
                if dispatch.try_recv().is_err() {
                    let _ = self.abandon.send(error.clone());
                }
                Err(error)
            }
        }
    }
}

async fn transport(
    mut ws: WebSocketStream<UnixStream>,
    mut outgoing: mpsc::UnboundedReceiver<Outbound>,
    respond: mpsc::UnboundedSender<Outbound>,
    events: mpsc::UnboundedSender<Event>,
    pending: Pending,
    mut abandoned: mpsc::UnboundedReceiver<Error>,
) {
    let error = tokio::select! {
        biased;
        Some(error)=abandoned.recv()=>error,
        error=pump(&mut ws, &mut outgoing, &respond, &events, &pending)=>error,
    };
    outgoing.close();
    for (_, tx) in pending.lock().unwrap().drain() {
        let _ = tx.send(Err(error.clone()));
    }
    let _ = events.send(Event::Disconnected(error));
}
async fn pump(
    ws: &mut WebSocketStream<UnixStream>,
    outgoing: &mut mpsc::UnboundedReceiver<Outbound>,
    respond: &mpsc::UnboundedSender<Outbound>,
    events: &mpsc::UnboundedSender<Event>,
    pending: &Pending,
) -> Error {
    let write_limit = Duration::from_secs(30);
    loop {
        tokio::select! {
            Some(Outbound{value,flushed}) = outgoing.recv() => {
                if value.get("method").is_some()
                    && value["id"].as_u64().is_some_and(|id| !pending.lock().unwrap().contains_key(&id)) {
                    continue;
                }
                match timeout(write_limit,ws.send(Message::text(value.to_string()))).await {
                    Ok(Ok(()))=>{if let Some(ack)=flushed{let _=ack.send(());}}
                    Ok(Err(error))=>break Error::transport(error),
                    Err(error)=>break Error::transport(error),
                }
            }
            packet = ws.next() => {
                let message = match packet {
                    Some(Ok(message)) => message,
                    Some(Err(error)) => break Error::transport(error),
                    None => break Error::transport("Shared Codex connection closed"),
                };
                match message {
                    Message::Close(_) => break Error::transport("Shared Codex connection closed"),
                    Message::Ping(_) => {
                        match timeout(write_limit,ws.flush()).await {
                            Ok(Ok(()))=>{},
                            Ok(Err(error))=>break Error::transport(error),
                            Err(error)=>break Error::transport(error),
                        }
                    }
                    Message::Text(_) | Message::Binary(_) => {
                        if let Err(error) = handle_message(message, respond, events, pending) {
                            let frame = CloseFrame { code: CloseCode::Invalid, reason: "Invalid JSON-RPC".into() };
                            let _ = timeout(write_limit,ws.close(Some(frame))).await;
                            break error;
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

fn handle_message(
    message: Message,
    respond: &mpsc::UnboundedSender<Outbound>,
    events: &mpsc::UnboundedSender<Event>,
    pending: &Pending,
) -> Result<()> {
    let packet: Value = serde_json::from_slice(&message.into_data())
        .map_err(|_| Error::transport("Failed to parse JSON from codex app-server"))?;
    if let Some(method) = packet["method"].as_str() {
        let params = packet["params"].clone();
        let event = match packet.get("id") {
            Some(id) if id.is_number() || id.is_string() => Event::Request(ServerRequest {
                id: id.clone(),
                method: method.into(),
                params,
                out: respond.clone(),
            }),
            Some(_) => return Ok(()),
            None => Event::Notification {
                method: method.into(),
                params,
            },
        };
        let _ = events.send(event);
    } else if let Some(id) = packet["id"].as_u64() {
        if let Some(reply) = pending.lock().unwrap().remove(&id) {
            let result = match packet["error"].as_object() {
                Some(error) => Err(Error::Rejected(
                    error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .into(),
                )),
                None => Ok(packet["result"].clone()),
            };
            let _ = reply.send(result);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::UnixListener;
    use tokio_tungstenite::accept_async;
    pub(super) async fn accept_initialize(ws: &mut WebSocketStream<UnixStream>) {
        let request: Value =
            serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
        assert_eq!(request["method"], "initialize");
        ws.send(Message::text(
            json!({"id":request["id"],"result":{}}).to_string(),
        ))
        .await
        .unwrap();
        let initialized: Value =
            serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
        assert_eq!(initialized, json!({"method":"initialized"}));
    }

    #[tokio::test]
    async fn multiplex_and_reconnect() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("engine.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let peer = tokio::spawn(async move {
            let mut ws = accept_async(listener.accept().await.unwrap().0)
                .await
                .unwrap();
            accept_initialize(&mut ws).await;
            let mut ids = vec![];
            for _ in 0..2 {
                let v: Value =
                    serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap())
                        .unwrap();
                assert!(v.get("jsonrpc").is_none());
                ids.push(v["id"].as_u64().unwrap());
            }
            for v in [
                json!({"id":ids[1],"error":{"message":"denied"}}),
                json!({"id":ids[0].to_string(),"result":"ignored"}),
                json!({"id":ids[0],"result":"ok"}),
                json!({"method":"update"}),
                json!({"id":"server","method":"approval"}),
            ] {
                ws.send(Message::text(v.to_string())).await.unwrap();
            }
            let reply: Value =
                serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
            assert_eq!(reply, json!({"id":"server","result":"approved"}));
            ws.next().await.unwrap().unwrap();
            ws.send(Message::text("invalid")).await.unwrap();
            drop(ws);
            let mut ws = accept_async(listener.accept().await.unwrap().0)
                .await
                .unwrap();
            let v: Value =
                serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
            assert_eq!(v["method"], "initialize");
            assert!(v["id"].as_u64().unwrap() > ids[1]);
            ws.send(Message::text(json!({"id":v["id"],"result":{}}).to_string()))
                .await
                .unwrap();
            let v: Value =
                serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
            assert_eq!(v, json!({"method":"initialized"}));
        });
        let mut client = Client::connect(&path, None).await.unwrap();
        assert!(matches!(
            client.request("a", Value::Null).await,
            Err(Error::NotReady(_))
        ));
        client.initialize().await.unwrap();
        let (a, b) = tokio::join!(
            client.request("a", Value::Null),
            client.request("b", Value::Null)
        );
        assert_eq!(a.unwrap(), "ok");
        assert_eq!(b.unwrap_err(), Error::Rejected("denied".into()));
        assert!(matches!(
            client.events.recv().await,
            Some(Event::Notification { .. })
        ));
        let Some(Event::Request(r)) = client.events.recv().await else {
            panic!()
        };
        r.respond(json!("approved")).unwrap();
        assert!(matches!(
            client.request("c", Value::Null).await,
            Err(Error::Indeterminate(_))
        ));
        assert!(matches!(
            client.events.recv().await,
            Some(Event::Disconnected(_))
        ));
        client.reconnect().await.unwrap();
        peer.await.unwrap();
    }
    #[tokio::test(start_paused = true)]
    async fn request_deadlines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("engine.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let peer = tokio::spawn(async move {
            let mut ws = accept_async(listener.accept().await.unwrap().0)
                .await
                .unwrap();
            accept_initialize(&mut ws).await;
            std::future::pending::<()>().await;
        });
        let mut client = Client::connect(&path, None).await.unwrap();
        client.initialize().await.unwrap();
        for (method, secs) in [("thread/read", 20), ("initialize", 30), ("model/list", 30)] {
            let start = tokio::time::Instant::now();
            assert!(matches!(
                client.request(method, json!({})).await,
                Err(Error::Indeterminate(_))
            ));
            assert_eq!(start.elapsed(), Duration::from_secs(secs));
            assert!(client.pending.lock().unwrap().is_empty());
        }
        peer.abort();
    }
}

#[cfg(test)]
#[tokio::test]
async fn stalled_dispatch_abandons_all_requests() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("engine.sock");
    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    let peer = tokio::spawn(async move {
        let mut ws = tokio_tungstenite::accept_async(listener.accept().await.unwrap().0)
            .await
            .unwrap();
        tests::accept_initialize(&mut ws).await;
        std::future::pending::<()>().await;
    });
    let mut client = Client::connect(&path, None).await.unwrap();
    client.initialize().await.unwrap();
    let (first, second) = tokio::join!(
        client.request_timeout("long", json!("x".repeat(4 << 20)), Duration::from_secs(5)),
        client.request_timeout("short", Value::Null, Duration::from_millis(200))
    );
    let err = second.unwrap_err();
    assert!(matches!(err, Error::Indeterminate(_)));
    assert_eq!(first.unwrap_err(), err);
    assert!(matches!(client.events.recv().await,Some(Event::Disconnected(e)) if e==err));
    assert!(matches!(
        client.request("late", Value::Null).await,
        Err(Error::NotReady(_))
    ));
    peer.abort();
}

#[cfg(test)]
#[tokio::test]
async fn bearer_and_capability_preconditions() {
    use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("engine.sock");
    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    let peer = tokio::spawn(async move {
        let stream = listener.accept().await.unwrap().0;
        let mut ws = tokio_tungstenite::accept_hdr_async(stream, |r: &Request, s: Response| {
            assert_eq!(r.headers()["Authorization"], "Bearer secret");
            Ok(s)
        })
        .await
        .unwrap();
        tests::accept_initialize(&mut ws).await;
        let packet = ws.next().await.unwrap().unwrap();
        let request: Value = serde_json::from_str(packet.to_text().unwrap()).unwrap();
        ws.send(Message::text(
            json!({"id":request["id"],"error":{"message":"invalid thread"}}).to_string(),
        ))
        .await
        .unwrap();
        ws.close(None).await.unwrap();
    });
    let mut client = Client::connect(&path, Some("secret")).await.unwrap();
    assert!(matches!(
        super::app_server::check_capabilities(&client).await,
        Err(Error::NotReady(_))
    ));
    client.initialize().await.unwrap();
    assert!(matches!(
        super::app_server::check_capabilities(&client).await,
        Err(Error::NotReady(_) | Error::Indeterminate(_))
    ));
    peer.await.unwrap();
    assert!(matches!(
        client.events.recv().await,
        Some(Event::Disconnected(_))
    ));
    assert!(matches!(
        super::app_server::check_capabilities(&client).await,
        Err(Error::NotReady(_))
    ));
}
