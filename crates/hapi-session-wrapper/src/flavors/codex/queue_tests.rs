use super::*;
use futures_util::FutureExt;
use std::{
    collections::HashMap,
    process::Stdio,
    sync::atomic::{AtomicU64, Ordering},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::{mpsc, oneshot},
};
type Replies = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>>;
struct Bridge {
    queue: u64,
    out: mpsc::UnboundedSender<Value>,
    replies: Replies,
    sequence: Arc<AtomicU64>,
}
impl Bridge {
    async fn call(&self, event: &str, args: Value) -> Result<Value> {
        let seq = self.sequence.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.replies.lock().unwrap().insert(seq, tx);
        self.out
            .send(json!({"queue":self.queue,"event":event,"args":args,"seq":seq}))
            .unwrap();
        rx.await.unwrap()
    }
    fn notify(&self, event: &str, args: Value) {
        self.out
            .send(json!({"queue":self.queue,"event":event,"args":args}))
            .unwrap();
    }
}
impl Host for Bridge {
    fn rpc<'a>(&'a self, m: &'a str, p: Value) -> BoxFuture<'a, Result<Value>> {
        self.call("rpc", json!([m, p])).boxed()
    }
    fn consumed(&self, ids: &[String], steered: bool) {
        self.notify(
            "consumed",
            if steered {
                json!([ids, true])
            } else {
                json!([ids])
            },
        );
    }
    fn mirror(&self, id: &str, input: Option<&Input>) {
        self.notify("mirror", json!([id, input]));
    }
    fn uncertain(&self, ids: &[String]) {
        self.notify("uncertain", json!([ids]));
    }
    fn requeued(&self, ids: Vec<String>) -> BoxFuture<'_, Result<()>> {
        async move { self.call("requeued", json!([ids])).await.map(|_| ()) }.boxed()
    }
}
#[tokio::test]
async fn original_queue_suites() {
    let mut child = tokio::process::Command::new("bun")
        .args(["test", "src/flavors/codex/queue_parity.test.ts"])
        .env("HAPI_RUST_QUEUE_TEST", "1")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let mut stdin = child.stdin.take().unwrap();
    let (out, mut rx) = mpsc::unbounded_channel::<Value>();
    let writer = tokio::spawn(async move {
        while let Some(v) = rx.recv().await {
            if stdin.write_all(format!("{v}\n").as_bytes()).await.is_err() {
                break;
            }
        }
    });
    let sequence = Arc::new(AtomicU64::new(1));
    let replies = Replies::default();
    let queues = Arc::new(Mutex::new(HashMap::<u64, Arc<Queue>>::new()));
    let mut count = 0;
    while let Some(line) = lines.next_line().await.unwrap() {
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if let Some(seq) = v["seq"].as_u64() {
            let tx = replies.lock().unwrap().remove(&seq).unwrap();
            let r = if let Some(s) = v["error"].as_str() {
                Err(if v["lost"] == true {
                    Error::Indeterminate(s.into())
                } else {
                    Error::Rejected(s.into())
                })
            } else {
                Ok(v["value"].clone())
            };
            let _ = tx.send(r);
            continue;
        }
        count += 1;
        let queue = v["queue"].as_u64().unwrap();
        let host = Arc::new(Bridge {
            queue,
            out: out.clone(),
            sequence: sequence.clone(),
            replies: replies.clone(),
        });
        let queues = queues.clone();
        tokio::spawn(async move {
            let args = &v["args"];
            let id = args[0].as_str().unwrap_or("");
            let method = v["method"].as_str().unwrap();
            let result: Result<Value> = async {
                if method == "load" {
                    let file = args[1].as_str().unwrap();
                    let q = Queue::load(id.into(), PathBuf::from(file), host.clone())?;
                    queues.lock().unwrap().insert(queue, Arc::new(q));
                    return Ok(Value::Null);
                }
                let q = queues.lock().unwrap()[&queue].clone();
                Ok(match method {
                    "enqueue" => {
                        q.enqueue(
                            id,
                            valid_input(&args[1])?,
                            args[2] == true,
                            args[3].as_str(),
                        )
                        .await?;
                        Value::Null
                    }
                    "committed" => json!(q.committed(id, &args[1])?),
                    "reconcile" => json!(q.reconcile().await?),
                    "suspend" => {
                        q.suspend().await?;
                        Value::Null
                    }
                    "deleted" => {
                        q.deleted(id).await?;
                        Value::Null
                    }
                    "cancel" => q.cancel(id).await?,
                    "steer" => {
                        q.steer(
                            id,
                            args[1].as_str().unwrap(),
                            args.get(2).and_then(Value::as_array).cloned(),
                        )
                        .await?
                    }
                    "nativeMutation" => q.native_mutation(id, args[1].clone()).await?,
                    "command" => json!(
                        q.command(id, || async {
                            Ok(host
                                .call("command", json!([id]))
                                .await?
                                .as_str()
                                .map(str::to_owned))
                        })
                        .await?
                    ),
                    _ => panic!("unexpected {method}"),
                })
            }
            .await;
            let states = queues.lock().unwrap().get(&queue).map(|q| {
                q.lock()
                    .entries
                    .iter()
                    .map(|(k, v)| (k.clone(), v.state))
                    .collect::<HashMap<_, _>>()
            });
            let mut msg = json!({"call":v["call"],"states":states});
            match result {
                Ok(val) => msg["value"] = val,
                Err(e) => msg["error"] = json!(e.to_string()),
            }
            host.out.send(msg).unwrap();
        });
    }
    assert!(child.wait().await.unwrap().success());
    assert!(count > 300);
    writer.abort();
}
proptest::proptest! {
    #[test]fn match_content_requires_one_eligible_version(a in ".{0,32}",b in ".{0,32}"){
        let version=|s:&str|Version::new(vec![Member{id:"a".into(),input:vec![json!({"type":"text","text":s})]}],None);
        let mut batch=Batch{client_id:"b".into(),native_id:"n".into(),key:"key".into(),state:Queued,current:1,versions:vec![version(&a),version(&b)]};
        let parts=vec![json!({"text":b,"type":"text","text_elements":[]})];proptest::prop_assert_eq!(batch.matches(&parts,false).len(),if a==b{2}else{1});
        batch.confirm(1);proptest::prop_assert_eq!(batch.matches(&parts,true),vec![1]);batch.versions[1].status=Status::Consumed;
        proptest::prop_assert!(batch.matches(&parts,false).is_empty());proptest::prop_assert_eq!(batch.matches(&parts,true),vec![1]);
    }
}

#[derive(Default)]
struct FaultEngine {
    outcome: Mutex<u8>,
    submissions: Mutex<Vec<Submission>>,
    adds: Mutex<HashMap<String, usize>>,
    journal: PathBuf,
    crash: Mutex<Option<Vec<u8>>>,
}
impl Host for FaultEngine {
    fn rpc<'a>(&'a self, method: &'a str, p: Value) -> BoxFuture<'a, Result<Value>> {
        async move {
            let mut native = self.submissions.lock().unwrap();
            match method {
                "thread/queue/list" => Ok(json!({"data":*native})),
                "thread/queue/add" => {
                    let outcome = *self.outcome.lock().unwrap();
                    let data = fs::read(&self.journal).unwrap();
                    let id = p["clientUserMessageId"].as_str().unwrap();
                    let written: Value = serde_json::from_slice(&data).unwrap();
                    assert_eq!(written["entries"][id]["state"], "unknown");
                    *self.crash.lock().unwrap() = Some(data);
                    if outcome == 1 {
                        return Err(Error::Rejected("definitely rejected".into()));
                    }
                    let sub = Submission {
                        id: format!("n-{id}"),
                        input: valid_input(&p["input"])?,
                        client_user_message_id: id.into(),
                    };
                    *self.adds.lock().unwrap().entry(id.into()).or_default() += 1;
                    native.push(sub.clone());
                    if outcome == 2 {
                        Err(Error::Indeterminate("lost acknowledgment".into()))
                    } else {
                        Ok(json!({"queuedSubmission":sub}))
                    }
                }
                _ => panic!("unexpected {method}"),
            }
        }
        .boxed()
    }
    fn consumed(&self, _: &[String], _: bool) {}
    fn mirror(&self, _: &str, _: Option<&Input>) {}
    fn uncertain(&self, _: &[String]) {}
    fn requeued(&self, _: Vec<String>) -> BoxFuture<'_, Result<()>> {
        async { Ok(()) }.boxed()
    }
}
proptest::proptest! {
    #![proptest_config(proptest::test_runner::Config::with_cases(32))]
    #[test]fn restores_journals_without_duplicate_dispatch(steps in proptest::collection::vec((0..4u8,0..3u8,proptest::bool::ANY,proptest::bool::ANY),1..24)){
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async{
            let dir=tempfile::tempdir().unwrap();let journal=dir.path().join("s.queue.json");let engine=Arc::new(FaultEngine{journal:journal.clone(),..Default::default()});
            let open=||Queue::load("thread".into(),journal.clone(),engine.clone()).unwrap();let mut q=open();
            for(n,fault,restart,consume)in steps{
                let id=format!("id-{n}");let input=vec![json!({"type":"text","text":id})];*engine.outcome.lock().unwrap()=fault;
                let _=q.enqueue(&id,input.clone(),false,None).await;
                let crash=engine.crash.lock().unwrap().take();if restart{if let Some(before_ack)=crash{fs::write(&journal,before_ack).unwrap();}q=open();}
                q.reconcile().await.unwrap();
                if consume && q.state(&id)==Some(Queued){
                    engine.submissions.lock().unwrap().retain(|s|s.client_user_message_id!=id);q.committed(&id,&json!(input)).unwrap();
                    q=open();assert_eq!(q.state(&id),Some(Consumed));
                }
                assert!(engine.adds.lock().unwrap().values().all(|count|*count==1));
            }
        });
    }
}

#[tokio::test]
async fn old_record_only_ledger_survives_upgrade() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("session.queue.json");
    let input = json!([{"type":"text","text":"legacy","extra":"keep"}]);
    fs::write(
        &file,
        json!({"old":{"state":"queued","nativeId":"n","input":input}}).to_string(),
    )
    .unwrap();
    let engine = Arc::new(FaultEngine {
        journal: file.clone(),
        ..Default::default()
    });
    let open = || Queue::load("t".into(), file.clone(), engine.clone());
    let q = open().unwrap();
    assert_eq!(q.state("old"), Some(Queued));
    q.committed("old", &input).unwrap();
    let durable: Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
    assert_eq!(durable["version"], 2);
    assert_eq!(durable["entries"]["old"]["input"], input);
    assert_eq!(open().unwrap().state("old"), Some(Consumed));
    let mut invalid = durable;
    invalid["entries"]["old"]["batchId"] = Value::Null;
    fs::write(&file, invalid.to_string()).unwrap();
    assert!(open().is_err());
}
