//! agy session bootstrap (TS `runAgy.ts`, `loop.ts`, `session.ts`).
//!
//! The version probe, `agy acp` spawn and ACP `initialize` run before the hub
//! session is bootstrapped: any failure there exits 86 so the runner retries
//! with the TypeScript wrapper (which keeps the stream-json fallback).
use super::{
    launcher::{Command, Launcher, Session},
    model_selection::{normalize_agy_model_selection, split_agy_model_selection},
    models,
    reactive_wakeup::default_agy_brain_dir,
    swarm_board::parse_agy_swarm_board_command,
    version,
};
use crate::flavors::common::format_message;
use hapi_wrapper_core::{
    acp::{
        backend::{Backend, BackendOptions},
        message_handler::TextChunkMode,
    },
    config::Config,
    lifecycle::{
        proctree,
        runner_lifecycle::{startup_fallback, RunnerLifecycle},
        shutdown_handlers::register_shutdown_handlers,
    },
    queue::Placement,
    rest::Rest,
    rpc::handlers::{GeneratedImages, WorkspaceHandlers},
    session::{
        bootstrap,
        client::{Cancellation, SessionEvent},
        metadata,
    },
    terminal::TerminalExtension,
};
use serde_json::{json, Value};
use std::{collections::HashMap, io, sync::Arc, time::Duration};
use tokio::sync::oneshot;

const AGY: &str = "agy";
const PERMISSION_MODES: [&str; 4] = ["request-review", "always-proceed", "acceptEdits", "plan"];

/// GEMINI_FORCE_FILE_STORAGE reads the saved OAuth token instead of a flaky
/// keyring; SSH markers would switch agy to a degraded auth path.
pub fn agy_env(source: impl Iterator<Item = (String, String)>) -> HashMap<String, String> {
    let mut env: HashMap<String, String> = source.collect();
    env.insert("GEMINI_FORCE_FILE_STORAGE".into(), "true".into());
    for k in ["SSH_CLIENT", "SSH_CONNECTION", "SSH_TTY"] {
        env.remove(k);
    }
    env
}

fn make_backend() -> Backend {
    Backend::new(BackendOptions {
        command: AGY.into(),
        args: vec!["acp".into()],
        env: Some(agy_env(std::env::vars())),
        cwd: None,
        flavor: Some("agy".into()),
        text_chunk_mode: TextChunkMode::Delta,
        elicitation_form: true,
        client_version: std::env::var("HAPI_WRAPPER_CLI_VERSION").unwrap_or_default(),
    })
}

fn nullable(v: &Value, key: &str) -> Result<Option<Option<String>>, String> {
    let Some(v) = v.get(key) else {
        return Ok(None);
    };
    let invalid = || format!("Invalid {key}");
    match v {
        Value::Null => Ok(Some(None)),
        Value::String(s) if !s.trim().is_empty() => Ok(Some(Some(s.trim().into()))),
        Value::Object(o) if key == "model" => o["modelId"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| Some(Some(s.to_owned())))
            .ok_or_else(invalid),
        _ => Err(invalid()),
    }
}

async fn config_rpc(s: &Session, v: Value) -> Result<Value, String> {
    if !v.is_object() {
        return Err("Invalid session config payload".into());
    }
    let mut applied = serde_json::Map::new();
    let permission = match v.get("permissionMode") {
        None => None,
        Some(p) => {
            let p = p
                .as_str()
                .filter(|p| PERMISSION_MODES.contains(p))
                .ok_or("Invalid permission mode")?;
            applied.insert("permissionMode".into(), json!(p));
            Some(p.to_owned())
        }
    };
    let model = nullable(&v, "model")?;
    if v.get("modelReasoningEffort").is_some() {
        return Err("Invalid model reasoning effort".into());
    }
    let effort = nullable(&v, "effort")?;
    if let Some(m) = &model {
        applied.insert("model".into(), json!(m));
    }
    if let Some(e) = &effort {
        applied.insert("effort".into(), json!(e));
    }
    let mut selection = None;
    if model.is_some() || effort.is_some() {
        let catalog = models::list_agy_models(false).await;
        let catalog = catalog["availableModels"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let current = s.mode.lock().unwrap().clone();
        let next_model = match &model {
            Some(m) => m.clone(),
            None => current["model"].as_str().map(str::to_owned),
        };
        let next_effort: Option<Option<String>> = match &effort {
            Some(e) => Some(e.clone()),
            None if model.is_some()
                && split_agy_model_selection(next_model.as_deref()).1.is_some() =>
            {
                None
            }
            None => Some(current["effort"].as_str().map(str::to_owned)),
        };
        selection = Some(normalize_agy_model_selection(
            next_model.as_deref(),
            next_effort.as_ref().map(|e| e.as_deref()),
            &catalog,
        )?);
    }
    {
        let mut mode = s.mode.lock().unwrap();
        if let Some(p) = permission {
            mode["permissionMode"] = json!(p);
        }
        if let Some((m, e)) = selection {
            mode["model"] = json!(m);
            mode["effort"] = json!(e);
            applied.insert("model".into(), json!(m));
            applied.insert("effort".into(), json!(e));
        }
    }
    s.keep_alive();
    Ok(json!({"applied":applied}))
}

fn user_message(s: &Session, v: Value, id: Option<String>) {
    let text = v["content"]["text"].as_str().unwrap_or("");
    let paths = v["content"]["attachments"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|a| a["path"].as_str().map(str::to_owned))
        .collect::<Vec<_>>();
    let text = format_message(text, &paths);
    let mode = s.runtime();
    let placement = if parse_agy_swarm_board_command(&text).is_some() {
        Placement::Isolated
    } else {
        Placement::Push
    };
    if s.queue
        .lock()
        .unwrap()
        .enqueue(placement, text, mode, id, None)
        .is_ok()
    {
        s.changed.notify_one();
    }
}

fn session_rpcs(s: &Arc<Session>) {
    let session = s.clone();
    s.client.rpc.register("set-session-config", move |v| {
        let session = session.clone();
        async move { config_rpc(&session, v).await.map(Some) }
    });
    let tx = s.tx.clone();
    s.client.rpc.register("steer-queued-message", move |v| {
        let tx = tx.clone();
        async move {
            let (reply, rx) = oneshot::channel();
            let id = v["localId"].as_str().unwrap_or("").to_owned();
            if tx.send(Command::Steer(id, reply)).is_err() {
                return Ok(Some(json!({"steered":false,"error":"Session ending"})));
            }
            Ok(Some(rx.await.unwrap_or_else(
                |_| json!({"steered":false,"error":"Session ending"}),
            )))
        }
    });
    let tx = s.tx.clone();
    s.client.rpc.register("listSlashCommands", move |_| {
        let tx = tx.clone();
        async move {
            let (reply, rx) = oneshot::channel();
            tx.send(Command::ListSlashCommands(reply))
                .map_err(|_| "Session closed")?;
            Ok(Some(rx.await.map_err(|_| "Session closed")?))
        }
    });
    let tx = s.tx.clone();
    s.client.rpc.register("abort", move |_| {
        let tx = tx.clone();
        async move {
            let (done, rx) = oneshot::channel();
            tx.send(Command::Abort(done))
                .map_err(|_| "Session closed")?;
            rx.await.map_err(|_| "Session closed")?;
            Ok(None)
        }
    });
    s.client.rpc.register("switch", |_| async { Ok(None) });
    let q = s.clone();
    s.client.on_cancel(Arc::new(move |id| {
        let result = q.queue.lock().unwrap().cancel_by_local_id(&id);
        if result == Cancellation::Removed {
            q.client.echoes.lock().unwrap().discard_id(&id);
        }
        Box::pin(async move { Ok(result) })
    }));
    let q = s.clone();
    s.client.on_retry(Arc::new(move |id| {
        let result = q.queue.lock().unwrap().release_indeterminate(&id);
        if result {
            q.changed.notify_one();
        }
        Box::pin(async move { Ok(result) })
    }));
}

/// Probe, spawn and `initialize` before the hub session exists.
async fn preflight() -> io::Result<Backend> {
    version::check(AGY).await.map_err(io::Error::other)?;
    let backend = make_backend();
    if let Err(e) = backend.initialize().await {
        backend.disconnect().await;
        return Err(io::Error::other(format!(
            "Failed to start agy acp (requires Antigravity CLI 1.5.0 or newer): {e}"
        )));
    }
    Ok(backend)
}

async fn start(opts: Value, known: &mut Option<String>) -> io::Result<i32> {
    let runner = opts["startedBy"] == "runner";
    if opts["startingMode"] == "local" {
        return Err(io::Error::other("Antigravity runs in remote mode only"));
    }
    for key in ["HAPI_WRAPPER_CLI_VERSION", "HAPI_WRAPPER_RUNTIME_PATH"] {
        if std::env::var_os(key).is_none_or(|v| v.is_empty()) {
            return Err(io::Error::other(format!("{key} is required")));
        }
    }
    proctree::establish_wrapper_group()?;
    let reserved = opts["reservedSessionId"].as_str().map(str::to_owned);
    let existing = opts["existingSessionId"].as_str().map(str::to_owned);
    // agy and its tools see the hub session id, as with the TS wrapper.
    if let Some(id) = existing.clone().or(reserved.clone()) {
        std::env::set_var("HAPI_SESSION_ID", id);
    }
    let backend = preflight().await?;
    let config = Config::load().await?;
    let cwd = metadata::invoked_cwd()?;
    let rest = Rest::new(&config).await?;
    let images = GeneratedImages::default();
    let model = opts["model"].as_str().map(str::to_owned);
    let effort = opts["effort"].as_str().map(str::to_owned);
    let options = bootstrap::Options {
        flavor: "agy".into(),
        runner,
        working_directory: Some(cwd.clone()),
        tag: Some(format!("__hapi_agy_{}", uuid::Uuid::new_v4())),
        agent_state: Some(json!({"controlledByUser":false,"startingMode":"remote"})),
        model: model.clone(),
        effort: effort.clone(),
        reserved_id: reserved,
        existing_id: existing,
        extensions: vec![
            Arc::new(WorkspaceHandlers {
                root: cwd.clone(),
                images: images.clone(),
            }),
            TerminalExtension::new(),
        ],
        ..Default::default()
    };
    let prepared = match bootstrap::bootstrap(&config, &options, true, false, |_| false).await {
        Ok(p) => p,
        Err(e) => {
            backend.disconnect().await;
            return Err(e);
        }
    };
    let client = prepared.client;
    let mut events = prepared.events;
    *known = Some(client.id.clone());
    for (key, value) in prepared.child_env {
        std::env::set_var(key, value);
    }
    let permission = opts["permissionMode"]
        .as_str()
        .unwrap_or("request-review")
        .to_owned();
    let (s, cmd) = Session::new(
        client.clone(),
        json!({"permissionMode":permission,"model":null,"effort":null}),
        cwd.to_string_lossy().into_owned(),
    );
    let lifecycle = RunnerLifecycle::new(client.clone(), Box::new(|_| Box::pin(async {})));
    let tx = s.tx.clone();
    // TS `onBeforeClose`: the launcher aborts the turn, stops agy and writes
    // its final state before the session is archived.
    lifecycle.set_before_close(Box::new(move |_| {
        let tx = tx.clone();
        Box::pin(async move {
            let (done, rx) = oneshot::channel();
            if tx.send(Command::Kill(done)).is_ok() {
                let _ = rx.await;
            }
        })
    }));
    lifecycle.register_process_handlers()?;
    register_shutdown_handlers(
        &client.rpc,
        &lifecycle,
        config.machine_id.clone(),
        None,
        client.archive_notifications(),
    );
    session_rpcs(&s);
    models::register(&client.rpc);
    let _ = rest
        .report_started(&config.home, &client.id, &prepared.metadata)
        .await;
    drop(rest);
    s.keep_alive();
    // The launch model must match the catalog before queued turns snapshot it.
    let (m, e) = if split_agy_model_selection(model.as_deref()).0.is_some() {
        let catalog = models::list_agy_models(false).await;
        let catalog = catalog["availableModels"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        normalize_agy_model_selection(model.as_deref(), Some(effort.as_deref()), &catalog)
            .unwrap_or((model.clone(), effort.clone()))
    } else {
        normalize_agy_model_selection(model.as_deref(), Some(effort.as_deref()), &[])
            .unwrap_or((None, effort.clone()))
    };
    {
        let mut mode = s.mode.lock().unwrap();
        mode["model"] = json!(m);
        mode["effort"] = json!(e);
    }
    s.keep_alive();
    let session = s.clone();
    client.on_user_message(Arc::new(move |v, id| user_message(&session, v, id)));
    let resume = opts["resumeSessionId"].as_str().map(str::to_owned);
    if let Some(id) = &resume {
        s.found(id);
    }
    let mut launcher = Launcher::new(
        s.clone(),
        cmd,
        backend,
        Box::new(make_backend),
        default_agy_brain_dir(),
    );
    let outcome = tokio::select! {
        result = launcher.run(resume) => Some(result),
        _ = async { while let Some(e) = events.recv().await { if matches!(e, SessionEvent::ConnectError { fatal: true, .. }) { break; } } } => None,
    };
    match outcome {
        // A stop (SIGTERM, killSession) keeps the lifecycle's own end reason.
        Some(Ok(())) if !launcher.killed() => lifecycle.set_session_end_reason("completed"),
        Some(Ok(())) => {}
        Some(Err(error)) => {
            eprintln!("{error}");
            lifecycle.mark_crash();
        }
        None => lifecycle.mark_crash(),
    }
    let _ = tokio::time::timeout(Duration::from_secs(1), launcher.cleanup()).await;
    Ok(lifecycle.cleanup().await)
}

pub async fn run(opts: Value) -> i32 {
    let mut known = None;
    match start(opts, &mut known).await {
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
    fn agy_env_forces_file_storage_and_drops_ssh_markers() {
        let env = agy_env(
            [
                ("SSH_TTY", "/dev/pts/1"),
                ("SSH_AUTH_SOCK", "/tmp/a"),
                ("PATH", "/bin"),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned())),
        );
        assert_eq!(env["GEMINI_FORCE_FILE_STORAGE"], "true");
        assert!(!env.contains_key("SSH_TTY"));
        assert_eq!(env["SSH_AUTH_SOCK"], "/tmp/a");
    }

    #[test]
    fn config_values_are_nullable_strings() {
        assert_eq!(nullable(&json!({"model":null}), "model"), Ok(Some(None)));
        assert_eq!(
            nullable(&json!({"model":{"modelId":" m "}}), "model"),
            Ok(Some(Some("m".into())))
        );
        assert!(nullable(&json!({"effort":""}), "effort").is_err());
        assert_eq!(nullable(&json!({}), "effort"), Ok(None));
    }
}
