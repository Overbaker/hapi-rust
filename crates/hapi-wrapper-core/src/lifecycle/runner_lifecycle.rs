use crate::session::{
    client::SessionClient,
    metadata::{now, spread},
};
use futures_util::future::BoxFuture;
use serde_json::json;
use std::{
    io,
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};
use tokio::{
    signal::unix::{signal, SignalKind},
    sync::watch,
    time::{timeout, Instant},
};

pub const STOP_BUDGET: Duration = Duration::from_millis(1500);
const REAP_MARGIN: Duration = Duration::from_millis(75);
pub const FLUSH_CAP: Duration = Duration::from_secs(1);
pub const STARTUP_FALLBACK: i32 = 86;
pub fn startup_fallback(session_id: Option<&str>) -> i32 {
    if let Some(id) = session_id {
        eprintln!("HAPI_RUST_WRAPPER_SESSION_ID={id}");
    }
    STARTUP_FALLBACK
}
trait Session: Send + Sync {
    fn prepare(&self, archive: &str, end: &str);
    fn flush(&self, limit: Duration) -> BoxFuture<'_, bool>;
    fn close(&self);
    fn shutdown_extensions(&self) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }
}
impl Session for SessionClient {
    fn prepare(&self, archive: &str, end: &str) {
        self.stop_keep_alive();
        let reason = archive.to_owned();
        self.metadata.update(move |current| {
            let mut next = current.clone();
            spread(
                &mut next,
                &json!({
                    "lifecycleState":"archived", "lifecycleStateSince":now(),
                    "archivedBy":"cli", "archiveReason":reason
                }),
            );
            next
        });
        self.session_end(Some(end));
    }
    fn flush(&self, limit: Duration) -> BoxFuture<'_, bool> {
        Box::pin(SessionClient::flush(self, limit))
    }
    fn shutdown_extensions(&self) -> BoxFuture<'_, ()> {
        SessionClient::shutdown_extensions(self)
    }
    fn close(&self) {
        SessionClient::close(self)
    }
}
struct State {
    archive: String,
    end: String,
    explicit: bool,
    code: i32,
    prepared: bool,
}
pub type AfterClose = Box<dyn Fn(Instant) -> BoxFuture<'static, ()> + Send + Sync>;
type FinishWork = Arc<dyn Fn(Instant, super::Deadline) -> BoxFuture<'static, ()> + Send + Sync>;
pub struct RunnerLifecycle {
    session: Arc<dyn Session>,
    state: Mutex<State>,
    exit: watch::Sender<bool>,
    closed: OnceLock<watch::Receiver<bool>>,
    deadline: watch::Sender<Option<Instant>>,
    shutdown: tokio::sync::OnceCell<i32>,
    finish_work: FinishWork,
}
impl RunnerLifecycle {
    pub fn new(session: Arc<SessionClient>, after_close: AfterClose) -> Arc<Self> {
        Self::for_wrapper(session, after_close)
    }
    fn for_wrapper(session: Arc<dyn Session>, after_close: AfterClose) -> Arc<Self> {
        Arc::new(Self::with_work(
            session,
            Arc::new(move |deadline, mut changes| {
                let cleanup = after_close(deadline);
                Box::pin(async move {
                    let now = Instant::now();
                    super::before(
                        now + deadline.saturating_duration_since(now) / 3,
                        Some(&mut changes),
                        cleanup,
                    )
                    .await;
                    let deadline = changes.borrow().map_or(deadline, |d| d.min(deadline));
                    let _ = super::proctree::kill_agent_tree(deadline).await;
                })
            }),
        ))
    }
    #[cfg(test)]
    fn with_session(session: Arc<dyn Session>, after_close: AfterClose) -> Self {
        Self::with_work(session, Arc::new(move |deadline, _| after_close(deadline)))
    }
    fn with_work(session: Arc<dyn Session>, finish_work: FinishWork) -> Self {
        Self {
            session,
            state: Mutex::new(State {
                archive: "Hub restart".into(),
                end: "terminated".into(),
                explicit: false,
                code: 0,
                prepared: false,
            }),
            exit: watch::channel(false).0,
            closed: Default::default(),
            shutdown: Default::default(),
            finish_work,
            deadline: watch::channel(None).0,
        }
    }
    pub fn set_exit_code(&self, code: i32) {
        self.state.lock().unwrap().code = code
    }
    pub fn set_archive_reason(&self, reason: &str) {
        self.state.lock().unwrap().archive = reason.into()
    }
    pub fn set_session_end_reason(&self, reason: &str) {
        let mut s = self.state.lock().unwrap();
        s.end = reason.into();
        s.explicit = true;
        if reason == "completed" && s.archive == "Hub restart" {
            s.archive = "Session completed".into();
        }
    }
    pub fn has_explicit_session_end_reason(&self) -> bool {
        self.state.lock().unwrap().explicit
    }
    pub fn mark_crash(&self) {
        let mut s = self.state.lock().unwrap();
        s.archive = "Session crashed".into();
        s.end = "error".into();
        s.code = 1;
    }
    pub fn request_exit(&self) {
        self.exit.send_replace(true);
    }
    fn prepare(&self) {
        let mut s = self.state.lock().unwrap();
        if !s.prepared {
            self.session.prepare(&s.archive, &s.end);
            s.prepared = true;
        }
    }
    fn tighten_deadline(&self, deadline: Instant) {
        self.deadline.send_if_modified(|old| {
            if old.is_none_or(|old| deadline < old) {
                *old = Some(deadline);
                true
            } else {
                false
            }
        });
    }
    async fn finish(&self, deadline: Instant) {
        self.tighten_deadline(deadline);
        let mut finished = self
            .closed
            .get_or_init(|| {
                let session = self.session.clone();
                let work = self.finish_work.clone();
                let changes = self.deadline.subscribe();
                let (tx, rx) = watch::channel(false);
                tokio::spawn(async move {
                    session.close();
                    work(deadline, changes.clone()).await;
                    let reap = session.shutdown_extensions();
                    let end = changes.borrow().unwrap_or(deadline) + REAP_MARGIN;
                    let _ = tokio::time::timeout_at(
                        end.min(Instant::now() + Duration::from_millis(250)),
                        reap,
                    )
                    .await;
                    tx.send_replace(true);
                });
                rx
            })
            .clone();
        let _ = finished.wait_for(|done| *done).await;
    }
    pub async fn cleanup_confirmed(&self, limit: Duration) -> io::Result<()> {
        if self.closed.get().is_some() {
            self.finish(Instant::now() + Duration::from_secs(3)).await;
            return Ok(());
        }
        self.prepare();
        if !timeout(limit, self.session.flush(limit))
            .await
            .unwrap_or(false)
        {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Timed out confirming session archive",
            ));
        }
        self.finish(Instant::now() + Duration::from_secs(3)).await;
        Ok(())
    }
    pub async fn cleanup(&self) -> i32 {
        *self
            .shutdown
            .get_or_init(|| async {
                let deadline = Instant::now() + STOP_BUDGET - REAP_MARGIN;
                self.tighten_deadline(deadline);
                if self.closed.get().is_none() {
                    self.prepare();
                    let _ = timeout(FLUSH_CAP, self.session.flush(FLUSH_CAP)).await;
                }
                self.finish(deadline).await;
                self.state.lock().unwrap().code
            })
            .await
    }
    pub fn register_process_handlers(self: &Arc<Self>) -> io::Result<()> {
        super::proctree::establish_wrapper_group()?;
        let mut term = signal(SignalKind::terminate())?;
        let mut int = signal(SignalKind::interrupt())?;
        let mut exit = self.exit.subscribe();
        let this = self.clone();
        tokio::spawn(async move {
            tokio::select! {_=term.recv()=>{},_=int.recv()=>{this.set_archive_reason("User terminated");},_=exit.wait_for(|v|*v)=>{}}
            std::process::exit(this.cleanup().await);
        });
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    #[derive(Default)]
    struct Mock {
        events: Mutex<Vec<String>>,
        ack: AtomicBool,
    }
    impl Session for Mock {
        fn prepare(&self, a: &str, e: &str) {
            self.events.lock().unwrap().extend([a.into(), e.into()]);
        }
        fn flush(&self, limit: Duration) -> BoxFuture<'_, bool> {
            self.events
                .lock()
                .unwrap()
                .push(format!("flush {}", limit.as_millis()));
            Box::pin(async move {
                if !self.ack.load(Ordering::Relaxed) {
                    std::future::pending::<()>().await
                }
                true
            })
        }
        fn close(&self) {
            self.events.lock().unwrap().push("close".into());
        }
    }
    fn fixture() -> (RunnerLifecycle, Arc<Mock>) {
        let m = Arc::new(Mock::default());
        (
            RunnerLifecycle::with_session(m.clone(), Box::new(|_| Box::pin(async {}))),
            m,
        )
    }
    #[tokio::test(start_paused = true)]
    async fn reasons_and_exit_codes() {
        for (action, a, e, code) in [
            (0, "Hub restart", "terminated", 0),
            (1, "User terminated", "terminated", 0),
            (2, "Session crashed", "error", 1),
            (3, "Session completed", "completed", 0),
            (4, "override", "completed", 0),
            (5, "Session crashed", "error", 1),
        ] {
            let (l, m) = fixture();
            match action {
                1 => l.set_archive_reason("User terminated"),
                2 => l.mark_crash(),
                3 => l.set_session_end_reason("completed"),
                4 => {
                    l.set_archive_reason("override");
                    l.set_session_end_reason("completed");
                }
                5 => {
                    l.set_session_end_reason("completed");
                    l.mark_crash();
                }
                _ => {}
            }
            assert_eq!(l.has_explicit_session_end_reason(), action >= 3);
            let start = Instant::now();
            assert_eq!(l.cleanup().await, code);
            assert!(start.elapsed() <= STOP_BUDGET);
            assert_eq!(*m.events.lock().unwrap(), [a, e, "flush 1000", "close"]);
        }
    }
    #[tokio::test(start_paused = true)]
    async fn confirmed_timeout_then_retry() {
        let (l, m) = fixture();
        let limit = Duration::from_secs(5);
        assert_eq!(
            l.cleanup_confirmed(limit).await.unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        m.ack.store(true, Ordering::Relaxed);
        l.cleanup_confirmed(limit).await.unwrap();
        l.cleanup().await;
        assert_eq!(
            *m.events.lock().unwrap(),
            [
                "Hub restart",
                "terminated",
                "flush 5000",
                "flush 5000",
                "close"
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn signal_shortens_existing_finish_even_if_confirmed_caller_is_dropped() {
        struct Ordered(Arc<Mutex<Vec<&'static str>>>);
        impl Session for Ordered {
            fn prepare(&self, _: &str, _: &str) {
                self.0.lock().unwrap().push("end");
            }
            fn flush(&self, _: Duration) -> BoxFuture<'_, bool> {
                Box::pin(async { true })
            }
            fn close(&self) {
                self.0.lock().unwrap().push("close");
            }
            fn shutdown_extensions(&self) -> BoxFuture<'_, ()> {
                self.0.lock().unwrap().push("reap");
                Box::pin(async {})
            }
        }
        let events = Arc::new(Mutex::new(vec![]));
        let log = events.clone();
        let started = Arc::new(tokio::sync::Notify::new());
        let notify = started.clone();
        let lifecycle = Arc::new(RunnerLifecycle::with_work(
            Arc::new(Ordered(events.clone())),
            Arc::new(move |deadline, mut changes| {
                let log = log.clone();
                notify.notify_one();
                Box::pin(async move {
                    super::super::before(
                        deadline,
                        Some(&mut changes),
                        std::future::pending::<()>(),
                    )
                    .await;
                    log.lock().unwrap().push("kill");
                })
            }),
        ));
        let l = lifecycle.clone();
        let first = tokio::spawn(async move { l.cleanup_confirmed(Duration::from_secs(5)).await });
        started.notified().await;
        first.abort();
        let start = Instant::now();
        assert_eq!(lifecycle.cleanup().await, 0);
        assert!(start.elapsed() < STOP_BUDGET);
        assert_eq!(*events.lock().unwrap(), ["end", "close", "kill", "reap"]);
    }
    #[tokio::test]
    async fn shutdown_rpc_targets_and_drains() {
        use super::super::shutdown_handlers::*;
        use crate::rpc::manager::RpcHandlerManager;
        let (l, _) = fixture();
        let l = Arc::new(l);
        let rpc = RpcHandlerManager::new("s");
        let (_tx, rx) = watch::channel(false);
        let ok = Arc::new(AtomicBool::new(false));
        let gate = ok.clone();
        register_shutdown_handlers(
            &rpc,
            &l,
            Some("m/1".into()),
            Some(Arc::new(move |_| {
                let ok = gate.load(Ordering::Relaxed);
                Box::pin(async move { Ok(ok) })
            })),
            rx,
        );
        let invoke = |method: String, p: serde_json::Value| {
            let rpc = &rpc;
            async move {
                serde_json::from_str::<serde_json::Value>(
                    &rpc.handle_request(
                        &json!({"method":format!("s:{method}"),"params":p.to_string()}),
                    )
                    .await
                    .unwrap(),
                )
                .unwrap()
            }
        };
        let pid = std::process::id();
        let scoped = format!("killSession/process/m%2F1/{pid}");
        let marker = super::super::start_marker::start_marker(pid).await.unwrap();
        let output = std::process::Command::new("bun")
            .current_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/../../cli"))
            .args(["-e","import {monotonicTimeMs} from './src/utils/time.ts'; console.log(monotonicTimeMs()+30000)"])
            .output().unwrap();
        assert!(output.status.success());
        let expires: u64 = String::from_utf8(output.stdout)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let mut r = json!({"expectedPid":pid,"expectedMachineId":"m/1","expectedProcessStartMarker":marker,"prepareOnly":true,"preparationLease":{"id":"a","expiresAt":expires}});
        assert_eq!(
            invoke(scoped.clone(), r.clone()).await["error"],
            "Session shutdown preparation was not confirmed"
        );
        ok.store(true, Ordering::Relaxed);
        let reply = invoke(scoped.clone(), r.clone()).await;
        assert_eq!(reply["preparationLease"], r["preparationLease"]);
        assert_eq!(reply["pid"], pid);
        assert!(!*l.exit.borrow());
        r["expectedProcessStartMarker"] = "wrong".into();
        assert!(invoke(scoped.clone(), r.clone()).await["error"]
            .as_str()
            .unwrap()
            .contains("generation"));
        r["prepareOnly"] = false.into();
        assert!(invoke(scoped, r).await["error"]
            .as_str()
            .unwrap()
            .contains("exact preparation target"));
        assert!(
            invoke("killSession".into(), json!({"prepareOnly":true})).await["error"]
                .as_str()
                .unwrap()
                .contains("lease")
        );
        assert!(
            invoke("killSession".into(), json!({"expectedMachineId":"other"})).await["error"]
                .as_str()
                .unwrap()
                .contains("machine")
        );
        let generation = format!("killSession/generation/m%2F1/{pid}");
        assert!(invoke(
            generation.clone(),
            json!({"expectedMachineId":"m/1","expectedPid":pid})
        )
        .await["error"]
            .as_str()
            .unwrap()
            .contains("exact preparation target"));
        assert!(invoke(
            generation.clone(),
            json!({"expectedMachineId":"m/1","expectedPid":pid,"expectedProcessStartMarker":"wrong"})
        )
        .await["error"]
            .as_str()
            .unwrap()
            .contains("generation"));
        assert!(!*l.exit.borrow());
        let killed = invoke(
            generation,
            json!({"expectedMachineId":"m/1","expectedPid":pid,"expectedProcessStartMarker":marker}),
        )
        .await;
        assert_eq!(killed["success"], true);
        assert_eq!(killed["machineId"], "m/1");
        assert_eq!(killed["processStartMarker"], marker);
        assert!(*l.exit.borrow());
        let plain = invoke("killSession".into(), json!({})).await;
        assert_eq!(plain["success"], true);
        assert_eq!(plain["machineId"], "m/1");
        assert_eq!(l.state.lock().unwrap().archive, "User terminated");
    }
    #[tokio::test]
    async fn hub_archive_before_and_after_registration() {
        for initial in [false, true] {
            let (l, _) = fixture();
            let l = Arc::new(l);
            let (tx, rx) = watch::channel(initial);
            super::super::shutdown_handlers::register_shutdown_handlers(
                &crate::rpc::manager::RpcHandlerManager::new("s"),
                &l,
                None,
                None,
                rx,
            );
            if !initial {
                assert!(!*l.exit.borrow());
                tx.send_replace(true);
                tokio::task::yield_now().await;
            }
            assert!(*l.exit.borrow());
            assert_eq!(l.state.lock().unwrap().archive, "User terminated");
        }
    }
    #[tokio::test]
    async fn handoff_ack_precedes_cleanup() {
        let (l, _) = fixture();
        let l = Arc::new(l);
        let (_tx, rx) = watch::channel(false);
        let rpc = crate::rpc::manager::RpcHandlerManager::new("s");
        super::super::shutdown_handlers::register_shutdown_handlers(&rpc, &l, None, None, rx);
        assert_eq!(
            rpc.handle_request(&json!({"method":"s:handoff-local"}))
                .await
                .unwrap(),
            r#"{"ok":true}"#
        );
        assert!(!*l.exit.borrow());
        l.exit.subscribe().wait_for(|v| *v).await.unwrap();
        assert_eq!(l.state.lock().unwrap().end, "handoff");
    }

    #[test]
    fn signal_deadline_with_stalled_flush_and_live_agent() {
        use nix::{
            sys::signal::{kill, Signal},
            unistd::Pid,
        };
        use std::{
            io::BufRead,
            process::{Command, Stdio},
        };
        const CHILD: &str = "HAPI_TEST_LIFECYCLE_SIGNAL";
        if let Ok(signal) = std::env::var(CHILD) {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                let session = Arc::new(Mock::default());
                let snapshot = session.clone();
                let l = RunnerLifecycle::for_wrapper(
                    session,
                    Box::new(move |_| {
                        let reason = if signal == "term" {
                            "Hub restart"
                        } else {
                            "User terminated"
                        };
                        assert_eq!(
                            *snapshot.events.lock().unwrap(),
                            [reason, "terminated", "flush 1000", "close"]
                        );
                        Box::pin(async {})
                    }),
                );
                l.register_process_handlers().unwrap();
                let mut child = Command::new("sh")
                    .args(["-c", "trap '' TERM; echo ready; exec sleep 60"])
                    .stdout(Stdio::piped())
                    .spawn()
                    .unwrap();
                let mut ready = String::new();
                std::io::BufReader::new(child.stdout.take().unwrap())
                    .read_line(&mut ready)
                    .unwrap();
                assert_eq!(ready, "ready\n");
                std::thread::spawn(move || child.wait().unwrap());
                println!("signal-ready");
                tokio::time::sleep(Duration::from_secs(8)).await;
                panic!("Signal handler did not exit");
            });
            return;
        }
        for signal in ["term", "int"] {
            let mut wrapper=Command::new(std::env::current_exe().unwrap())
                .args(["--exact","--nocapture","lifecycle::runner_lifecycle::tests::signal_deadline_with_stalled_flush_and_live_agent"])
                .env(CHILD,signal).stdout(Stdio::piped()).spawn().unwrap();
            let pid = Pid::from_raw(wrapper.id() as i32);
            let pgid = Pid::from_raw(-pid.as_raw());
            let mut lines = std::io::BufReader::new(wrapper.stdout.take().unwrap()).lines();
            assert!(lines.any(|s| s.unwrap().ends_with("signal-ready")));
            let start = Instant::now();
            kill(
                pid,
                if signal == "term" {
                    Signal::SIGTERM
                } else {
                    Signal::SIGINT
                },
            )
            .unwrap();
            assert!(wrapper.wait().unwrap().success());
            assert!(start.elapsed() <= STOP_BUDGET, "{:?}", start.elapsed());
            assert_eq!(kill(pgid, None), Err(nix::errno::Errno::ESRCH));
        }
    }
}
