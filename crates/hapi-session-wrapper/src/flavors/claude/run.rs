use super::{
    attachments, history,
    launcher::{Launcher, Session},
    query::{Event, Options, Query},
    remote::{catalog_args, strings, Launch},
    special_commands::{self, SpecialCommand},
    tool_result_images::ToolResultImages,
};
use hapi_wrapper_core::{
    config::Config,
    hooks::{
        server::{HookServer, Options as HookOptions},
        settings,
    },
    lifecycle::{
        proctree,
        runner_lifecycle::{startup_fallback, RunnerLifecycle},
        shutdown_handlers::register_shutdown_handlers,
    },
    mcp::tools,
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
use std::{
    io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

fn normalize(v: &Value, effort: bool) -> Option<String> {
    let s = v.as_str()?.trim();
    let normalized = if effort {
        s.to_lowercase()
    } else {
        s.to_owned()
    };
    (!matches!(normalized.as_str(), "" | "auto" | "default")).then_some(normalized)
}
fn executable() -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let cmd = std::env::var("HAPI_CLAUDE_PATH").unwrap_or_default();
    let cmd = cmd.trim();
    let cmd = if cmd.is_empty() { "claude" } else { cmd };
    let good = |p: &Path| {
        p.metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    if (cmd.contains('/') && good(Path::new(cmd)))
        || (!cmd.contains('/')
            && std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
                .any(|d| good(&d.join(cmd))))
    {
        Ok(())
    } else {
        Err(io::Error::other(
            "Claude Code CLI not found on PATH. Install Claude Code or set HAPI_CLAUDE_PATH.",
        ))
    }
}
fn project(cwd: &Path) -> io::Result<PathBuf> {
    let root = std::env::var_os("CLAUDE_CONFIG_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::home_dir().map(|h| h.join(".claude")))
        .ok_or_else(|| io::Error::other("Missing home directory"))?;
    let mut resolved = PathBuf::new();
    for component in std::env::current_dir()?.join(cwd).components() {
        match component {
            std::path::Component::ParentDir => {
                resolved.pop();
            }
            std::path::Component::CurDir => {}
            other => resolved.push(other),
        }
    }
    let project: String = resolved
        .to_string_lossy()
        .encode_utf16()
        .map(|u| {
            if u < 128 && (u as u8).is_ascii_alphanumeric() {
                u as u8 as char
            } else {
                '-'
            }
        })
        .collect();
    Ok(root.join("projects").join(project))
}

fn user_message(s: &Session, v: Value, id: Option<String>) {
    let text = v["content"]["text"].as_str().unwrap_or("");
    let paths = v["content"]["attachments"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|a| a["path"].as_str().map(str::to_owned))
        .collect::<Vec<_>>();
    let mut mode = {
        let mut mode = s.mode.lock().unwrap();
        for k in [
            "fallbackModel",
            "customSystemPrompt",
            "appendSystemPrompt",
            "allowedTools",
            "disallowedTools",
        ] {
            if let Some(value) = v["meta"].get(k) {
                if value.is_null() || value == "" {
                    mode.as_object_mut().unwrap().remove(k);
                } else {
                    mode[k] = value.clone();
                }
            }
        }
        mode.clone()
    };
    let (text, placement) = match special_commands::parse(text) {
        Some(SpecialCommand::Compact(_) | SpecialCommand::Clear) => {
            (text.trim().into(), Placement::IsolateAndClear)
        }
        Some(SpecialCommand::Plan {
            mode: permission,
            prompt,
            ..
        }) => {
            s.set_mode(permission);
            s.keep_alive();
            s.client.send_session_event(
                json!({"type":"permission-mode-changed","mode":permission}),
                None,
                None,
            );
            mode["permissionMode"] = permission.into();
            let Some(p) = prompt.filter(|p| !p.is_empty()) else {
                if let Some(id) = id {
                    s.client.messages_consumed(&[id], false, false);
                }
                return;
            };
            (attachments::format_message(p, &paths), Placement::Push)
        }
        None => {
            let expanded = s.expand(text, &attachments::format_message("", &paths));
            (
                if expanded == text {
                    attachments::format_message(text, &paths)
                } else {
                    expanded
                },
                Placement::Push,
            )
        }
    };
    s.enqueue(text, mode, id, placement);
}
fn config_rpc(s: &Session, v: Value) -> Result<Value, String> {
    if !v.is_object() {
        return Err("Invalid session config payload".into());
    }
    let permission = v
        .get("permissionMode")
        .map(|p| {
            p.as_str()
                .filter(|s| super::permission::valid_mode(s))
                .ok_or("Invalid permission mode")
        })
        .transpose()?;
    let field = |key: &str| {
        v.get(key)
            .map(|val| {
                if val.is_null() || val.is_string() {
                    Ok(json!(normalize(val, key == "effort")))
                } else {
                    Err(format!("Invalid {key}"))
                }
            })
            .transpose()
    };
    let model = field("model")?;
    let effort = field("effort")?;
    if let Some(p) = permission {
        s.set_mode(p);
    }
    let mut mode = s.mode.lock().unwrap();
    if let Some(model) = model {
        mode["model"] = model;
    }
    if let Some(effort) = effort {
        mode["effort"] = effort;
    }
    let result = json!({"applied":{"permissionMode":mode["permissionMode"],"model":mode["model"],"effort":mode["effort"]}});
    drop(mode);
    if ["permissionMode", "effort", "model"]
        .iter()
        .any(|k| v.get(k).is_some())
    {
        s.keep_alive();
    }
    Ok(result)
}
fn session_rpcs(s: &Arc<Session>) {
    let session = s.clone();
    s.client.rpc.register("set-session-config", move |v| {
        let result = config_rpc(&session, v);
        async move { result.map(Some) }
    });
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
        Box::pin(async move { Ok(result) })
    }));
}
struct Catalog {
    cwd: PathBuf,
    args: Vec<String>,
    discovered: Vec<Value>,
    cached: tokio::sync::Mutex<Option<Value>>,
}
impl Catalog {
    async fn load(&self, s: &Session) -> Value {
        let mut cache = self.cached.lock().await;
        if let Some(v) = cache.as_ref() {
            return v.clone();
        }
        let opts = Options {
            cwd: Some(self.cwd.clone()),
            additional_args: catalog_args(&self.args),
            allowed_tools: vec!["Bash(echo)".into()],
            max_turns: Some(1),
            ..Default::default()
        };
        let init = tokio::time::timeout(Duration::from_secs(10), async {
            let mut q = Query::spawn(&opts, Some("hello")).ok()?;
            while let Some(Ok(e)) = q.events.recv().await {
                if let Event::Message(v) = e {
                    if v["type"] == "system" && v["subtype"] == "init" {
                        let _ = q.input.abort();
                        return Some(v);
                    }
                }
            }
            None
        })
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
        let tools = init.get("tools").cloned();
        let Some(names) = init["slash_commands"].as_array() else {
            if let Some(t) = tools {
                s.client.metadata.update(move |m| {
                    let mut m = m.clone();
                    m["tools"] = t.clone();
                    m
                });
            }
            return json!({"success":false,"error":"Claude skill catalog unavailable"});
        };
        let mut commands = Vec::new();
        let mut skills = Vec::new();
        for name in names
            .iter()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let local = name.rsplit(':').next().unwrap();
            let d = self
                .discovered
                .iter()
                .find(|d| d["name"] == name)
                .or_else(|| self.discovered.iter().find(|d| d["name"] == local));
            if init["skills"]
                .as_array()
                .map_or(d.is_some(), |skills| skills.iter().any(|s| s == name))
            {
                let mut skill = json!({"name":name});
                if let Some(desc) = d.and_then(|d| d.get("description")) {
                    skill["description"] = desc.clone();
                }
                skills.push(skill);
            } else {
                commands.push(name.to_owned());
            }
        }
        *s.skills.lock().unwrap() = skills
            .iter()
            .filter_map(|v| v["name"].as_str().map(str::to_owned))
            .collect();
        s.client.metadata.update(move |m| {
            let mut m = m.clone();
            m["slashCommands"] = json!(commands);
            if let Some(t) = &tools {
                m["tools"] = t.clone();
            } else {
                m.as_object_mut().unwrap().remove("tools");
            }
            m
        });
        let result = json!({"success":true,"skills":skills});
        *cache = Some(result.clone());
        result
    }
}
async fn start(opts: Value, known: &mut Option<String>) -> io::Result<i32> {
    let runner = opts["startedBy"] == "runner";
    if opts["startingMode"] == "local" || (!runner && opts["startingMode"] != "remote") {
        return Err(io::Error::other(
            "Local Claude sessions require the TypeScript wrapper",
        ));
    }
    for key in ["HAPI_WRAPPER_CLI_VERSION", "HAPI_WRAPPER_RUNTIME_PATH"] {
        if std::env::var_os(key).is_none_or(|v| v.is_empty()) {
            return Err(io::Error::other(format!("{key} is required")));
        }
    }
    proctree::establish_wrapper_group()?;
    executable()?;
    let config = Config::load().await?;
    let cwd = metadata::invoked_cwd()?;
    let project = project(&cwd)?;
    let rest = Rest::new(&config).await?;
    let args = strings(&opts["claudeArgs"]);
    let images = GeneratedImages::default();
    let options = bootstrap::Options {
        flavor: "claude".into(),
        runner,
        working_directory: Some(cwd.clone()),
        model: normalize(&opts["model"], false),
        effort: normalize(&opts["effort"], true),
        reserved_id: opts["reservedSessionId"].as_str().map(str::to_owned),
        existing_id: opts["existingSessionId"].as_str().map(str::to_owned),
        extensions: vec![
            Arc::new(WorkspaceHandlers {
                root: cwd.clone(),
                images: images.clone(),
            }),
            TerminalExtension::new(),
        ],
        ..Default::default()
    };
    let prepared = bootstrap::bootstrap(&config, &options, true, false, |_| false).await?;
    let client = prepared.client;
    let mut events = prepared.events;
    *known = Some(client.id.clone());
    for (key, value) in prepared.child_env {
        std::env::set_var(key, value);
    }
    let mode = json!({"permissionMode":opts["permissionMode"].as_str().unwrap_or("default"),"model":options.model,"effort":options.effort});
    let (s, cmd) = Session::new(client.clone(), mode);
    if !args.iter().any(|arg| arg == "--fork-session") {
        *s.id.lock().unwrap() = opts["resumeSessionId"]
            .as_str()
            .or_else(|| prepared.session["metadata"]["claudeSessionId"].as_str())
            .map(str::to_owned);
    }
    let mcp = tools::start(
        client.clone(),
        tools::Options {
            working_directory: Some(cwd.clone()),
            generated_images: images.clone(),
            ..Default::default()
        },
    )
    .await?;
    let session = s.clone();
    let hooks = HookServer::start(HookOptions {
        token: None,
        on_pre_tool_use: None,
        on_permission_request: None,
        on_session_hook: Arc::new(move |id, v| {
            if v.get("agent_id").is_some() {
                return;
            }
            if v["hook_event_name"] == "PreCompact"
                && v["trigger"] == "auto"
                && session.id.lock().unwrap().as_deref() == Some(id)
            {
                session.notice("📦 Compaction started");
            }
            if (v.get("hook_event_name").is_none() || v["hook_event_name"] == "SessionStart")
                && session.id.lock().unwrap().as_deref() != Some(id)
            {
                session.found(id.into());
            }
        }),
    })
    .await?;
    let settings = settings::generate(
        &config.home,
        hooks.port,
        &hooks.token,
        settings::Options {
            filename_prefix: "session-hook",
            ..Default::default()
        },
    )
    .await?;
    let supports_resume_at = history::probe_resume_at().await;
    let launch = Launch {
        cwd: cwd.clone(),
        project,
        settings: settings.clone(),
        mcp_url: format!("http://127.0.0.1:{}/", mcp.port),
        args: args.clone(),
        summary: prepared.session_summary_contract.unwrap_or(false),
        supports_resume_at,
    };
    let resources = Mutex::new(Some((mcp, hooks)));
    let lifecycle = RunnerLifecycle::new(
        client.clone(),
        Box::new(move |_| {
            let servers = resources.lock().unwrap().take();
            let path = settings.clone();
            Box::pin(async move {
                let _ = settings::cleanup(&path).await;
                if let Some((m, h)) = servers {
                    tokio::join!(m.shutdown(), h.stop());
                }
            })
        }),
    );
    lifecycle.register_process_handlers()?;
    register_shutdown_handlers(
        &client.rpc,
        &lifecycle,
        config.machine_id.clone(),
        None,
        client.archive_notifications(),
    );
    session_rpcs(&s);
    client.metadata.update(move |m| {
        let mut m = m.clone();
        let mut capabilities = json!({"forkCurrent":true});
        if supports_resume_at {
            capabilities["forkAtMessage"] = true.into();
            capabilities["rewindToMessage"] = true.into();
        }
        m["capabilities"]["conversationHistory"] = capabilities;
        m
    });
    client.agent_state.update(|s| {
        let mut s = s.clone();
        s["controlledByUser"] = false.into();
        if s["startingMode"] != "pty" {
            s["startingMode"] = "remote".into();
        }
        s
    });
    client.metadata.update(|m| {
        let mut m = m.clone();
        if m["startingMode"] != "pty" {
            m["startingMode"] = "remote".into();
        }
        m
    });
    let mut launcher = Launcher::new(
        s.clone(),
        launch,
        &config.home,
        cmd,
        ToolResultImages::new(images),
    );
    let discovered = client
        .rpc
        .handle_request(&json!({"method":format!("{}:listSkills",client.id),"params":"{}"}))
        .await
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .unwrap_or_default();
    let catalog = Arc::new(Catalog {
        cwd,
        args,
        discovered: discovered["skills"].as_array().cloned().unwrap_or_default(),
        cached: Default::default(),
    });
    let (cat, session) = (catalog.clone(), s.clone());
    client.rpc.register("listSkills", move |_| {
        let (cat, s) = (cat.clone(), session.clone());
        async move { Ok(Some(cat.load(&s).await)) }
    });
    let session = s.clone();
    tokio::spawn(async move {
        catalog.load(&session).await;
        let s = session.clone();
        session
            .client
            .on_user_message(Arc::new(move |v, id| user_message(&s, v, id)));
    });
    let _ = rest
        .report_started(&config.home, &client.id, &prepared.metadata)
        .await;
    drop(rest);
    s.keep_alive();
    tokio::select! {
        result = launcher.run() => match result {
            Ok(()) => lifecycle.set_session_end_reason("completed"),
            Err(error) => { eprintln!("{error}"); lifecycle.mark_crash(); }
        },
        _=async{while let Some(e)=events.recv().await{if matches!(e,SessionEvent::ConnectError{fatal:true,..}){break;}}}=>lifecycle.mark_crash(),
    }
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

#[test]
fn config_normalization() {
    assert_eq!(normalize(&json!(" Default "), true), None);
    assert_eq!(normalize(&json!(" Opus "), false).as_deref(), Some("Opus"));
    assert_eq!(normalize(&json!(" HIGH "), true).as_deref(), Some("high"));
}

#[test]
fn project_is_resolved_lexically() {
    for cwd in ["/home/repo/", "/home/a/../repo", "/../../home//./repo"] {
        assert!(project(Path::new(cwd)).unwrap().ends_with("-home-repo"));
    }
    assert_eq!(
        project(Path::new(".")).unwrap(),
        project(&std::env::current_dir().unwrap()).unwrap()
    );
}
