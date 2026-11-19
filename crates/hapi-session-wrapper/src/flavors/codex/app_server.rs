use super::{
    app_server_client::{Client, Error},
    launch::Launch,
    registry::mkdir,
};
use hapi_wrapper_core::lifecycle::{proctree::terminate_child_tree, start_marker::start_marker};
use serde_json::json;
use std::{
    io,
    os::unix::fs::PermissionsExt,
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
    child: Option<Child>,
    pub pid: u32,
    pub marker: String,
    sockets: TempDir,
}
impl Drop for Server {
    fn drop(&mut self) {
        if let Some(child) = self.child.take() {
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn_blocking(move || stop_child(child));
            } else {
                drop(stop_child(child));
            }
        }
    }
}
fn stop_child(mut child: Child) -> Child {
    if let Some(pid) = child.id() {
        if !matches!(terminate_child_tree(pid), Ok(true)) {
            let _ = child.start_kill();
        }
    }
    child
}

impl Server {
    pub async fn spawn(command: &Path, launch: &Launch, home: &Path) -> io::Result<Self> {
        mkdir(home)?;
        let sockets = tempfile::Builder::new()
            .prefix("hapi-cx-")
            .rand_bytes(6)
            .tempdir_in("/tmp")?;
        std::fs::set_permissions(sockets.path(), std::fs::Permissions::from_mode(0o700))?;
        let endpoint = format!("unix://{}", sockets.path().join("engine.sock").display());
        let child = Command::new(command)
            .arg("app-server")
            .args(&launch.server_args)
            .args(["--listen", &endpoint])
            .current_dir(&launch.cwd)
            .env("CODEX_HOME", home.canonicalize()?)
            .env_remove("HAPI_SESSION_ID")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let pid = child
            .id()
            .ok_or_else(|| io::Error::other("Cannot verify app-server generation"))?;
        let mut server = Self {
            child: Some(child),
            pid,
            marker: String::new(),
            sockets,
        };
        server.marker = start_marker(pid)
            .await
            .ok_or_else(|| io::Error::other("Cannot verify app-server generation"))?;
        Ok(server)
    }
    pub fn socket(&self) -> PathBuf {
        self.sockets.path().join("engine.sock")
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
    pub async fn stop(mut self) -> io::Result<()> {
        let child = self.child.take().unwrap();
        let mut child = tokio::task::spawn_blocking(move || stop_child(child)).await?;
        child.wait().await?;
        Ok(())
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
#[cfg(test)]
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
            assert_eq!(unsafe { libc::getpgid(server.pid as i32) }, unsafe {
                libc::getpgrp()
            });
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
            server.stop().await.unwrap();
            assert!(!socket.exists());
            assert!(
                std::fs::read_to_string(format!("/proc/{}/stat", pid.trim()))
                    .map_or(true, |stat| stat.contains(") Z "))
            );
        }
    }
}
