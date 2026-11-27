use hapi_wrapper_core::jsjson::parse as parse_json;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    io,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{ChildStdin, ChildStdout},
    sync::{mpsc, oneshot},
};

type Reply = oneshot::Sender<Result<Value, String>>;
#[derive(Clone)]
pub struct Transport {
    writer: Arc<tokio::sync::Mutex<ChildStdin>>,
    sequence: Arc<AtomicU64>,
    pending: Arc<Mutex<HashMap<String, Reply>>>,
}
impl Transport {
    pub fn new(stdin: ChildStdin, stdout: ChildStdout) -> (Self, mpsc::UnboundedReceiver<Value>) {
        let transport = Self {
            writer: Arc::new(tokio::sync::Mutex::new(stdin)),
            sequence: Default::default(),
            pending: Default::default(),
        };
        let pending = transport.pending.clone();
        let (send, recv) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(v) = parse_json::<Value>(line.as_bytes()) else {
                    continue;
                };
                if v["type"] == "response" {
                    if let Some(tx) = v["id"]
                        .as_str()
                        .and_then(|id| pending.lock().unwrap().remove(id))
                    {
                        let _ = tx.send(if v["success"] == true {
                            Ok(v["data"].clone())
                        } else {
                            Err(v["error"].as_str().unwrap_or("Pi RPC failed").into())
                        });
                        continue;
                    }
                }
                if send.send(v).is_err() {
                    break;
                }
            }
            pending.lock().unwrap().clear();
        });
        (transport, recv)
    }
    pub async fn send(&self, v: Value) -> io::Result<()> {
        let mut data = serde_json::to_vec(&v)?;
        data.push(b'\n');
        self.writer.lock().await.write_all(&data).await
    }
    pub async fn rpc(&self, mut v: Value) -> Result<Value, String> {
        let id = self.sequence.fetch_add(1, Ordering::Relaxed).to_string();
        v["id"] = json!(id);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id.clone(), tx);
        let seconds = if v["type"] == "compact" { 120 } else { 10 };
        let result = tokio::time::timeout(Duration::from_secs(seconds), async {
            self.send(v).await.map_err(|e| e.to_string())?;
            rx.await.map_err(|_| "Pi exited".to_owned())?
        })
        .await
        .unwrap_or_else(|_| Err("Pi RPC timed out".into()));
        self.pending.lock().unwrap().remove(&id);
        result
    }
}
