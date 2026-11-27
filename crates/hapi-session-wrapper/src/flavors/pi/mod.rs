mod events;
mod history;
mod maintenance;
mod prompt;
mod runtime;
mod transport;
mod ui;

use hapi_wrapper_core::{
    config::Config,
    lifecycle::{
        proctree,
        runner_lifecycle::{startup_fallback, RunnerLifecycle},
        shutdown_handlers::register_shutdown_handlers,
    },
    mcp::tools,
    rest::Rest,
    rpc::handlers::{GeneratedImages, WorkspaceHandlers},
    session::{bootstrap, client::SessionEvent, metadata},
    terminal::TerminalExtension,
};
use serde_json::{json, Value};
use std::{
    io::{self, Write},
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::process::Command;

pub fn parse(args: &[String]) -> Result<Value, String> {
    let mut v = json!({"flavor":"pi"});
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let key = match a.as_str() {
            "--started-by" => "startedBy",
            "--hapi-starting-mode" => "startingMode",
            "--existing-session-id" => "existingSessionId",
            "--hapi-session-id" => "reservedSessionId",
            "--resume" | "-s" | "--session" | "--session-id" => "resumeSessionId",
            "--model" => "model",
            "--effort" => "effort",
            "--model-reasoning-effort" => "modelReasoningEffort",
            "--permission-mode" => return Err("Invalid --permission-mode value".into()),
            _ => continue,
        };
        let value = it
            .next()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| format!("Missing {a} value"))?;
        if (key == "startingMode" && !matches!(value.as_str(), "local" | "remote"))
            || (key == "existingSessionId" && value.starts_with('-'))
        {
            return Err(format!("Invalid {a} value"));
        }
        v[key] = json!(value);
    }
    Ok(v)
}

fn supported(s: &str) -> bool {
    s.split_whitespace()
        .find_map(|v| {
            let mut parts = v.trim_start_matches('v').split('.');
            Some((
                parts.next()?.parse::<u32>().ok()?,
                parts.next()?.parse::<u32>().ok()?,
            ))
        })
        .is_some_and(|v| v >= (0, 83))
}
async fn start(opts: &Value, known: &mut Option<String>) -> io::Result<i32> {
    if opts["startingMode"] == "local" {
        return Err(io::Error::other(
            "Local mode requires the TypeScript wrapper",
        ));
    }
    proctree::establish_wrapper_group()?;
    let version = tokio::time::timeout(
        Duration::from_secs(5),
        Command::new("pi")
            .arg("--version")
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(io::Error::other)??;
    if !version.status.success() || !supported(&String::from_utf8_lossy(&version.stdout)) {
        return Err(io::Error::other("Pi 0.83 or newer is required"));
    }
    let config = Config::load().await?;
    let cwd = metadata::invoked_cwd()?;
    let cli_version = std::env::var("HAPI_WRAPPER_CLI_VERSION").map_err(io::Error::other)?;
    let dir = config.home.join("runtime").join(cli_version).join("pi");
    std::fs::create_dir_all(&dir)?;
    let extension = dir.join("hapi-extension.ts");
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(include_bytes!("hapi-extension.ts"))?;
    tmp.persist(&extension)?;
    let images = GeneratedImages::default();
    let runner = opts["startedBy"] == "runner";
    let options = bootstrap::Options {
        runner,
        flavor: "pi".into(),
        working_directory: Some(cwd.clone()),
        existing_id: opts["existingSessionId"].as_str().map(str::to_owned),
        reserved_id: opts["reservedSessionId"].as_str().map(str::to_owned),
        extensions: vec![
            Arc::new(WorkspaceHandlers {
                root: cwd.clone(),
                images: images.clone(),
            }),
            TerminalExtension::new(),
        ],
        ..Default::default()
    };
    let mut b = bootstrap::bootstrap(&config, &options, true, false, |_| false).await?;
    let c = b.client;
    *known = Some(c.id.clone());
    for (k, v) in b.child_env {
        std::env::set_var(k, v)
    }
    let mut command = Command::new("pi");
    command
        .current_dir(&cwd)
        .args(["--mode", "rpc", "--extension"])
        .arg(extension)
        .env_remove("HAPI_PI_MCP_URL")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    if let Some(id) = opts["resumeSessionId"].as_str() {
        command.args(["--session", id]);
    }
    let disabled = std::env::var("HAPI_DISABLE_PEER_TOOLS")
        .is_ok_and(|s| matches!(s.to_lowercase().as_str(), "true" | "1" | "yes"));
    let server = if disabled {
        None
    } else {
        let server = tools::start(
            c.clone(),
            tools::Options {
                enable_change_title: false,
                enabled_tools: Some(
                    ["list_peers", "inspect_peer", "ping_peer", "spawn_peer"]
                        .map(str::to_owned)
                        .to_vec(),
                ),
                working_directory: Some(cwd),
                ..Default::default()
            },
        )
        .await?;
        command.env(
            "HAPI_PI_MCP_URL",
            format!("http://127.0.0.1:{}/", server.port),
        );
        Some(server)
    };
    let mut child = proctree::spawn_child(&mut command)?;
    let (t, events) =
        transport::Transport::new(child.stdin.take().unwrap(), child.stdout.take().unwrap());
    let server = Mutex::new(server);
    let lifecycle = RunnerLifecycle::new(
        c.clone(),
        Box::new(move |_| {
            let server = server.lock().unwrap().take();
            Box::pin(async move {
                if let Some(server) = server {
                    server.shutdown().await;
                }
            })
        }),
    );
    lifecycle.register_process_handlers()?;
    register_shutdown_handlers(
        &c.rpc,
        &lifecycle,
        config.machine_id.clone(),
        None,
        c.archive_notifications(),
    );
    let mut runtime = runtime::Runtime::new(c.clone(), t, events, images);
    c.agent_state.update(|s| {
        let mut s = s.clone();
        s["controlledByUser"] = false.into();
        s["startingMode"] = "remote".into();
        s
    });
    c.metadata.update(|m| {
        let mut m = m.clone();
        m["startingMode"] = "remote".into();
        m
    });
    let rest = Rest::new(&config).await?;
    let _ = rest.report_started(&config.home, &c.id, &b.metadata).await;
    let mut closed = c.closed_notifications();
    let result = tokio::select! {
        res=async{runtime.init(opts).await?;runtime.run().await}=>res,
        _=closed.wait_for(|done|*done)=>Ok(()),
        _=async{while let Some(e)=b.events.recv().await{if matches!(e,SessionEvent::ConnectError{fatal:true,..}|SessionEvent::HubArchived){break}}}=>Ok(())
    };
    if let Err(reason) = result {
        eprintln!("{reason}");
        lifecycle.mark_crash();
        lifecycle.set_archive_reason(&reason)
    }
    let code = lifecycle.cleanup().await;
    let _ = child.start_kill();
    let _ = child.wait().await;
    Ok(code)
}
pub async fn run(opts: Value) -> i32 {
    let mut known = None;
    match start(&opts, &mut known).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("{e}");
            startup_fallback(known.as_deref())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn version_boundary() {
        assert!(!supported("0.82.9"));
        assert!(supported("0.83.0"));
        assert!(supported("pi v1.0.0"));
        assert!(!supported("unknown"))
    }
    #[test]
    fn reservation_and_resume() {
        let v = parse(&["--resume", "native", "--hapi-session-id", "reserved"].map(str::to_owned))
            .unwrap();
        assert_eq!(
            v,
            json!({"flavor":"pi","resumeSessionId":"native","reservedSessionId":"reserved"})
        );
        assert!(parse(&["--permission-mode", "default"].map(str::to_owned)).is_err());
    }
}
