use super::{
    app_server_client::{Client, Error},
    launch::Launch,
    registry::mkdir,
};
use hapi_wrapper_core::lifecycle::{proctree::terminate_child_tree, start_marker::start_marker};
use serde_json::json;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::{
    io,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tempfile::TempDir;
use tokio::{
    process::{Child, Command},
    time::{sleep, timeout},
};
const METHODS: [&str; 10] = [
    "thread/queue/list",
    "thread/queue/add",
    "thread/queue/update",
    "thread/queue/delete",
    "thread/queue/start",
    "turn/steer",
    "thread/settings/update",
    "thread/metadata/update",
    "thread/fork",
    "thread/turns/list",
];

pub struct Server {
    #[cfg(windows)]
    endpoint: super::transport::Endpoint,
    child: Option<Child>,
    pub pid: u32,
    pub marker: String,
    sockets: Option<TempDir>,
}
impl Drop for Server {
    fn drop(&mut self) {
        if let (Some(child), Some(sockets)) = (self.child.take(), self.sockets.take()) {
            if let Err(error) = stop_child(child, sockets, terminate_child_tree) {
                eprintln!("{error}");
            }
        }
    }
}
const STOP_FAILED: &str = "Codex engine did not stop; ownership retained for orphan recovery";

fn stop_child(
    mut child: Child,
    sockets: TempDir,
    terminate: impl FnOnce(u32) -> io::Result<bool>,
) -> io::Result<(Child, TempDir)> {
    if child.id().is_none()
        || matches!(child.try_wait(), Ok(Some(_)))
        || child
            .id()
            .is_some_and(|pid| matches!(terminate(pid), Ok(true)))
    {
        return Ok((child, sockets));
    }
    let _ = sockets.keep();
    Err(io::Error::other(STOP_FAILED))
}

impl Server {
    pub async fn spawn(command: &Path, launch: &Launch, home: &Path) -> io::Result<Self> {
        mkdir(home)?;
        #[cfg(unix)]
        let sockets = tempfile::Builder::new()
            .prefix("hapi-cx-")
            .rand_bytes(6)
            .tempdir_in("/tmp")?;
        #[cfg(unix)]
        std::fs::set_permissions(sockets.path(), std::fs::Permissions::from_mode(0o700))?;
        #[cfg(unix)]
        let endpoint = format!("unix://{}", sockets.path().join("engine.sock").display());
        #[cfg(windows)]
        let sockets = tempfile::Builder::new().prefix("hapi-cx-").tempdir()?;
        #[cfg(windows)]
        let engine = super::transport::Endpoint {
            addr: std::net::TcpListener::bind(("127.0.0.1", 0))?.local_addr()?,
            token: super::transport::token(),
        };
        #[cfg(windows)]
        let endpoint = format!("ws://{}", engine.addr);
        let mut cmd = Command::new(command);
        hapi_wrapper_core::lifecycle::proctree::configure_child_group(&mut cmd);
        cmd.arg("app-server")
            .args(&launch.server_args)
            .args(["--listen", &endpoint]);
        #[cfg(windows)]
        cmd.args([
            "--ws-auth",
            "capability-token",
            "--ws-token-sha256",
            &super::registry::auth_hash(&engine.token),
        ]);
        let mut child = cmd
            .current_dir(&launch.cwd)
            .env("CODEX_HOME", home.canonicalize()?)
            .env_remove("HAPI_SESSION_ID")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        let pid = child
            .id()
            .ok_or_else(|| io::Error::other("Cannot verify app-server generation"))?;
        if let Err(error) = hapi_wrapper_core::lifecycle::proctree::track_child(pid) {
            let _ = child.kill().await;
            return Err(error);
        }
        let mut server = Self {
            #[cfg(windows)]
            endpoint: engine,
            child: Some(child),
            pid,
            marker: String::new(),
            sockets: Some(sockets),
        };
        let marker = tokio::select! {
            status = server.child.as_mut().unwrap().wait() => {
                return Err(io::Error::other(format!("app-server exited ({})", status?)));
            }
            marker = start_marker(pid) => marker,
        };
        server.verify_marker(marker).await
    }
    async fn verify_marker(mut self, marker: Option<String>) -> io::Result<Self> {
        if let Some(marker) = marker {
            self.marker = marker;
            Ok(self)
        } else {
            self.stop().await?;
            Err(io::Error::other("Cannot verify app-server generation"))
        }
    }
    #[cfg(windows)]
    pub fn socket(&self) -> super::transport::Endpoint {
        self.endpoint.clone()
    }
    #[cfg(windows)]
    pub async fn wait_exit(&mut self) -> String {
        loop {
            match self.child.as_mut().unwrap().try_wait() {
                Ok(None) => sleep(Duration::from_millis(100)).await,
                Ok(Some(status)) => return format!("app-server exited ({status})"),
                Err(error) => return error.to_string(),
            }
        }
    }
    #[cfg(unix)]
    pub fn socket(&self) -> PathBuf {
        self.sockets.as_ref().unwrap().path().join("engine.sock")
    }
    pub async fn initialize(&mut self) -> io::Result<Client> {
        let path = self.socket();
        let initialize = async {
            let client = timeout(Duration::from_secs(20), async {
                loop {
                    if let Ok(mut client) = Client::connect(&path, None).await {
                        if client.initialize().await.is_ok() {
                            return client;
                        }
                    }
                    sleep(Duration::from_millis(100)).await;
                }
            })
            .await
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "Timed out initializing Codex app-server",
                )
            })?;
            check_capabilities(&client)
                .await
                .map_err(io::Error::other)?;
            Ok(client)
        };
        tokio::select! {
            status = self.child.as_mut().unwrap().wait() => {
                Err(io::Error::other(format!("app-server exited ({})", status?)))
            },
            result = initialize => result,
        }
    }
    pub async fn stop(self) -> io::Result<()> {
        self.stop_with(terminate_child_tree).await
    }
    async fn stop_with(
        mut self,
        terminate: impl FnOnce(u32) -> io::Result<bool> + Send + 'static,
    ) -> io::Result<()> {
        let child = self.child.take().unwrap();
        let sockets = self.sockets.take().unwrap();
        let (mut child, sockets) =
            tokio::task::spawn_blocking(move || stop_child(child, sockets, terminate)).await??;
        match timeout(Duration::from_secs(1), child.wait()).await {
            Ok(Ok(_)) => Ok(()),
            _ => {
                let _ = sockets.keep();
                Err(io::Error::other(STOP_FAILED))
            }
        }
    }
}

pub async fn check_capabilities(client: &Client) -> Result<(), Error> {
    for method in METHODS {
        match client
            .request_timeout(
                method,
                json!({"threadId":"__hapi_capability_probe__"}),
                Duration::from_secs(30),
            )
            .await
        {
            Err(Error::Rejected(message)) => {
                let lower = message.to_lowercase();
                if ["method not found", "unknown method", "unsupported"]
                    .iter()
                    .any(|s| lower.contains(s))
                {
                    return Err(Error::Rejected(format!(
                        "Codex lacks {method}; upgrade to a compatible >= 0.154.0 build"
                    )));
                }
            }
            Err(error) => return Err(error),
            Ok(_) => {}
        }
    }
    Ok(())
}
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use serde_json::Value;
    use tokio::net::UnixListener;
    use tokio_tungstenite::{accept_async, tungstenite::Message};

    #[tokio::test]
    async fn spawn_probe_and_tree_cleanup() {
        for unsupported in [false, true] {
            let tmp = tempfile::tempdir().unwrap();
            let command = Path::new(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/src/flavors/codex/test_engine.sh"
            ));
            let home = tmp.path().join("home");
            let launch = Launch {
                cwd: tmp.path().into(),
                server_args: vec!["--strict-config".into()],
                tui_args: vec![],
                thread_params: json!({}),
            };
            let mut server = Server::spawn(command, &launch, &home).await.unwrap();
            assert_eq!(
                unsafe { libc::getpgid(server.pid as i32) },
                server.pid as i32
            );
            timeout(Duration::from_secs(5), async {
                while !tmp.path().join("ready").exists() {
                    sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            let socket = server.socket();
            let listener = UnixListener::bind(&socket).unwrap();
            let peer = tokio::spawn(async move {
                let mut ws = accept_async(listener.accept().await.unwrap().0)
                    .await
                    .unwrap();
                for method in ["initialize", "initialized"].into_iter().chain(METHODS) {
                    let packet = ws.next().await.unwrap().unwrap();
                    let v: Value = serde_json::from_str(packet.to_text().unwrap()).unwrap();
                    assert_eq!(v["method"], method);
                    let reply = match method {
                        "initialized" => continue,
                        "initialize" => {
                            assert_eq!(
                                v["params"],
                                json!({"clientInfo":{"name":"hapi","title":"HAPI","version":"1"},"capabilities":{"experimentalApi":true}})
                            );
                            json!({"id":v["id"],"result":{}})
                        }
                        _ => {
                            assert_eq!(
                                v["params"],
                                json!({"threadId":"__hapi_capability_probe__"})
                            );
                            json!({"id":v["id"],"error":{"message":if unsupported && method=="thread/queue/update"{"Unknown method"}else{"invalid thread id"}}})
                        }
                    };
                    ws.send(Message::text(reply.to_string())).await.unwrap();
                    if unsupported && method == "thread/queue/update" {
                        break;
                    }
                }
            });
            let result = server.initialize().await;
            if unsupported {
                assert!(result
                    .err()
                    .unwrap()
                    .to_string()
                    .starts_with("Codex lacks thread/queue/update"));
            } else {
                assert!(result.is_ok());
            }
            peer.await.unwrap();
            assert_eq!(
                std::fs::read_to_string(tmp.path().join("argv")).unwrap(),
                format!(
                    "app-server\n--strict-config\n--listen\nunix://{}\n",
                    socket.display()
                )
            );
            assert_eq!(
                std::fs::read_to_string(tmp.path().join("environment")).unwrap(),
                format!("{}\nunset\n", home.display())
            );
            let pid = std::fs::read_to_string(tmp.path().join("descendant")).unwrap();
            let pid: u32 = pid.trim().parse().unwrap();
            assert!(pid > 0 && Path::new(&format!("/proc/{pid}")).exists());
            server.stop().await.unwrap();
            assert!(!socket.exists());
            assert!(std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .map_or(true, |stat| stat.contains(") Z ")));
        }
    }
}

#[cfg(all(test, unix))]
#[tokio::test]
async fn failed_tree_sweep_retains_engine_for_recovery() {
    for failed in [
        Ok(false),
        Err(io::Error::from(io::ErrorKind::PermissionDenied)),
    ] {
        let child = Command::new("sleep").arg("120").spawn().unwrap();
        let pid = child.id().unwrap();
        #[cfg(unix)]
        let sockets = tempfile::tempdir().unwrap();
        let path = sockets.path().to_owned();
        let server = Server {
            pid,
            child: Some(child),
            sockets: Some(sockets),
            marker: String::new(),
        };
        let result = server.stop_with(move |_| failed).await;
        let live = unsafe { libc::kill(pid as i32, 0) } == 0;
        terminate_child_tree(pid).unwrap();
        assert_eq!(result.unwrap_err().to_string(), STOP_FAILED);
        assert!(live && path.is_dir());
        std::fs::remove_dir(path).unwrap();
    }
}

#[cfg(all(test, unix))]
#[tokio::test]
async fn cleanup_completes_before_marker_failure_or_drop_returns() {
    for marker_failure in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let launch = Launch {
            cwd: dir.path().into(),
            server_args: vec![],
            tui_args: vec![],
            thread_params: json!({}),
        };
        let server = Server::spawn(
            Path::new(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/src/flavors/codex/test_engine.sh"
            )),
            &launch,
            &dir.path().join("home"),
        )
        .await
        .unwrap();
        timeout(Duration::from_secs(5), async {
            while !dir.path().join("ready").exists() {
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let pid: u32 = std::fs::read_to_string(dir.path().join("descendant"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(pid > 0 && Path::new(&format!("/proc/{pid}")).exists());
        let sockets = server.socket().parent().unwrap().to_owned();
        if marker_failure {
            assert_eq!(
                server.verify_marker(None).await.err().unwrap().to_string(),
                "Cannot verify app-server generation"
            );
        } else {
            drop(server);
        }
        assert!(!sockets.exists());
        assert!(std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .map_or(true, |v| v.contains(") Z ")));
    }
}

#[cfg(all(test, unix))]
#[tokio::test(start_paused = true)]
async fn stop_requires_reaping_the_engine() {
    let child = Command::new("sleep").arg("120").spawn().unwrap();
    let pid = child.id().unwrap();
    let sockets = tempfile::tempdir().unwrap();
    let path = sockets.path().to_owned();
    let server = Server {
        child: Some(child),
        pid,
        marker: String::new(),
        sockets: Some(sockets),
    };
    let e = server.stop_with(|_| Ok(true)).await.unwrap_err();
    terminate_child_tree(pid).unwrap();
    assert_eq!(e.to_string(), STOP_FAILED);
    assert!(path.exists());
    std::fs::remove_dir(path).unwrap();
}
