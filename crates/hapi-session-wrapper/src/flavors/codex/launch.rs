use serde_json::{json, Value};
use std::path::{Component, Path, PathBuf};

#[derive(Debug)]
pub struct Launch {
    pub cwd: PathBuf,
    pub server_args: Vec<String>,
    pub tui_args: Vec<String>,
    pub thread_params: Value,
}

fn resolve(base: &Path, path: &str) -> PathBuf {
    let mut out = PathBuf::new();
    for part in base.join(path).components() {
        match part {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            _ => out.push(part),
        }
    }
    out
}
fn key(value: &str) -> &str {
    value
        .split_once('=')
        .map_or("", |(k, _)| k.trim().trim_matches(['\'', '"']))
}

pub fn take_reserved_session_id(options: &mut Value) -> Option<String> {
    options
        .as_object_mut()?
        .remove("reservedSessionId")?
        .as_str()
        .map(str::to_owned)
}

pub fn shared_launch_config(options: &Value, cwd: &Path) -> Result<Launch, String> {
    let args: Vec<&str> = options["codexArgs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let mut server = vec![];
    let mut tui = vec![];
    let mut params = json!({});
    let mut config = json!({});
    let mut directories = vec![];
    let base = std::path::absolute(cwd).map_err(|e| e.to_string())?;
    let cwd = base.as_path();
    let mut directory = resolve(cwd, ".");
    let mut oss = false;
    let mut provider = None;
    let mut auto = false;
    let mut model = options["model"].as_str();
    let mut iter = args.into_iter();
    while let Some(raw) = iter.next() {
        if raw == "--" {
            tui.push(raw);
            tui.extend(iter);
            break;
        }
        let (arg, inline) = if let Some((a, v)) = raw.split_once('=').filter(|(a, _)| !a.is_empty())
        {
            (a, Some(v))
        } else if raw.len() > 2 && raw.starts_with('-') && b"cmiCasp".contains(&raw.as_bytes()[1]) {
            (&raw[..2], Some(&raw[2..]))
        } else {
            (raw, None)
        };
        let mut value = || {
            inline
                .or_else(|| iter.next())
                .filter(|v| {
                    !v.is_empty()
                        && *v != "--"
                        && (!v.starts_with('-') || matches!(arg, "-c" | "--config"))
                })
                .ok_or_else(|| format!("Missing {arg} value"))
        };
        const PROFILE_ERROR: &str = "Codex app-server 0.154 does not support profile selection. Use CODEX_HOME with the desired config.toml or explicit -c overrides.";
        match arg {
            "-p" | "--profile" | "--config-profile" => return Err(PROFILE_ERROR.into()),
            a if a.starts_with("--remote") => {
                return Err("HAPI owns --remote and its authentication options".into())
            }
            "-c" | "--config" | "--enable" | "--disable" => {
                let v = value()?;
                server.extend([arg.to_owned(), v.into()]);
                if !matches!(arg, "-c" | "--config")
                    || !matches!(
                        key(v),
                        "approval_policy" | "sandbox_mode" | "approvals_reviewer"
                    )
                {
                    tui.extend([arg, v]);
                }
            }
            "-m" | "--model" => {
                let v = value()?;
                model = Some(v);
                tui.extend(["--model", v]);
            }
            "-s" | "--sandbox" => {
                let v = value()?;
                if !matches!(v, "read-only" | "workspace-write" | "danger-full-access") {
                    return Err(format!("Invalid {arg} value"));
                }
                params["sandbox"] = json!(v);
            }
            "-a" | "--ask-for-approval" => {
                let v = value()?;
                if !matches!(v, "untrusted" | "on-failure" | "on-request" | "never") {
                    return Err(format!("Invalid {arg} value"));
                }
                params["approvalPolicy"] = json!(v);
            }
            "-C" | "--cd" => directory = resolve(cwd, value()?),
            "--search" => {
                config["web_search"] = json!("live");
                tui.push(arg);
            }
            "--add-dir" => directories.push(value()?),
            "--oss" => oss = true,
            "--local-provider" => {
                let v = value()?;
                if !matches!(v, "ollama" | "lmstudio") {
                    return Err("Unsupported --local-provider".into());
                }
                provider = Some(v);
            }
            "--approve-for-me" | "--not-so-yolo" => auto = true,
            "--dangerously-bypass-hook-trust" => config["bypass_hook_trust"] = json!(true),
            "--strict-config" => server.push(arg.into()),
            "-i" | "--image" => tui.extend(["--image", value()?]),
            "--no-alt-screen" => tui.push(arg),
            "--yolo" | "--dangerously-bypass-approvals-and-sandbox" => {
                params["approvalPolicy"] = json!("never");
                params["sandbox"] = json!("danger-full-access");
            }
            "--full-auto" => {
                params["approvalPolicy"] = json!("on-request");
                params["sandbox"] = json!("workspace-write");
            }
            a if a.starts_with('-') => {
                return Err(format!("Unsupported shared Codex launch flag: {a}"))
            }
            _ => tui.push(raw),
        }
    }
    if oss && provider.is_none() {
        return Err(
            "--oss requires --local-provider (ollama or lmstudio) for a shared app-server".into(),
        );
    }
    if !directories.is_empty() {
        config["sandbox_workspace_write.writable_roots"] = json!(directories
            .into_iter()
            .map(|d| resolve(&directory, d))
            .collect::<Vec<_>>());
    }
    if let Some(p) = provider {
        config["model_provider"] = json!(p);
        params["modelProvider"] = json!(p);
    }
    if auto {
        params["approvalPolicy"] = json!("on-request");
        params["approvalsReviewer"] = json!("auto_review");
        params["sandbox"] = json!("workspace-write");
    }
    if let Some(mode) = options["permissionMode"].as_str() {
        let (approval, sandbox) = match mode {
            "default" | "autoReview" => ("on-request", "workspace-write"),
            "read-only" => ("never", "read-only"),
            "yolo" => ("never", "danger-full-access"),
            _ => return Err(format!("Unknown permission mode: {mode}")),
        };
        let reviewer = if mode == "autoReview" {
            "auto_review"
        } else {
            "user"
        };
        for (p, k, v) in [
            ("approvalPolicy", "approval_policy", approval),
            ("sandbox", "sandbox_mode", sandbox),
            ("approvalsReviewer", "approvals_reviewer", reviewer),
        ] {
            params[p] = json!(v);
            config[k] = json!(v);
        }
    }
    for (k, v) in config.as_object().unwrap() {
        server.extend(["-c".into(), format!("{k}={v}")]);
    }
    if !server
        .windows(2)
        .any(|p| matches!(p[0].as_str(), "-c" | "--config") && key(&p[1]) == "model_catalog_json")
    {
        if let Ok(path) = std::env::var("HAPI_CODEX_MODEL_CATALOG_JSON") {
            if !path.trim().is_empty() {
                let file =
                    if let Some(p) = path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
                        resolve(
                            Path::new(&std::env::var("HOME").map_err(|e| e.to_string())?),
                            p.trim_start_matches('/'),
                        )
                    } else {
                        PathBuf::from(path)
                    };
                if !file.is_absolute() {
                    return Err("HAPI_CODEX_MODEL_CATALOG_JSON must be an absolute path (or start with ~/).".into());
                }
                server.extend(["-c".into(), format!("model_catalog_json={}", json!(file))]);
            }
        }
    }
    if let Some(m) = model.filter(|m| !m.is_empty()) {
        params["model"] = json!(m);
    }
    if let Some(t) = options["serviceTier"].as_str().filter(|s| !s.is_empty()) {
        params["serviceTier"] = if t == "fast" {
            json!("priority")
        } else {
            Value::Null
        };
    }
    if let Some(e) = options["modelReasoningEffort"]
        .as_str()
        .filter(|s| !s.is_empty())
    {
        config["model_reasoning_effort"] = json!(e);
    }
    params["config"] = config;
    Ok(Launch {
        cwd: directory,
        server_args: server,
        tui_args: tui.into_iter().map(String::from).collect(),
        thread_params: params,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn takes_reservation_once() {
        let mut options = json!({"reservedSessionId": "reserved"});
        assert_eq!(
            take_reserved_session_id(&mut options).as_deref(),
            Some("reserved")
        );
        assert_eq!(take_reserved_session_id(&mut options), None);
    }
    #[test]
    fn launch_matches_typescript() {
        let mut cases = vec![
            json!({}),
            json!({"serviceTier":"fast"}),
            json!({"serviceTier":"standard"}),
        ];
        for mode in ["default", "read-only", "yolo", "autoReview"] {
            for flags in ["--yolo","--dangerously-bypass-approvals-and-sandbox","--full-auto","-sdanger-full-access -aon-request","--sandbox=read-only --ask-for-approval=untrusted","--approve-for-me","--not-so-yolo","-c sandbox_mode=\"danger-full-access\" -c approval_policy=\"never\" -c approvals_reviewer=\"auto_review\""]{
                let args:Vec<_>=flags.split_whitespace().chain(["--no-alt-screen","--","--yolo"]).collect();
                cases.push(json!({"permissionMode":mode,"codexArgs":args}));
            }
        }
        for flags in ["-C project -mcustom -a never --search --add-dir ../extra -c model_reasoning_effort=\"high\"","-pwork","--profile=work","--remote=ws://other","--worktree","--oss","-m","-- --profile=explain","-- -C literal","--oss --local-provider ollama --approve-for-me"]{
            cases.push(json!({"codexArgs":flags.split_whitespace().collect::<Vec<_>>()}));
        }
        let result=std::process::Command::new("bun").current_dir(concat!(env!("CARGO_MANIFEST_DIR"),"/../../cli")).args(["-e","import {sharedLaunchConfig as launch} from './src/codex/shared/launch'; console.log(JSON.stringify(JSON.parse(process.argv[1]).map(o=>{try{return launch(o,'/tmp')}catch(e){return e.message}})))",&json!(cases).to_string()]).output().unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let expected: Vec<Value> = serde_json::from_slice(&result.stdout).unwrap();
        for (options, want) in cases.iter().zip(expected) {
            let actual = match shared_launch_config(options, Path::new("/tmp")) {
                Ok(l) => {
                    json!({"cwd":l.cwd,"serverArgs":l.server_args,"threadParams":l.thread_params,"tuiArgs":l.tui_args})
                }
                Err(e) => json!(e),
            };
            assert_eq!(actual, want, "{options}");
        }
    }
}

#[cfg(test)]
#[test]
fn model_catalog_environment() {
    if let Ok(case) = std::env::var("HAPI_TEST_RUST_CATALOG_CASE") {
        let flags = if case == "override" {
            json!(["-c", "model_catalog_json=\"/from-cli\""])
        } else {
            json!([])
        };
        let result = shared_launch_config(&json!({"codexArgs":flags}), Path::new("/tmp"));
        match case.as_str() {
            "override" => assert_eq!(
                result.unwrap().server_args,
                ["-c", "model_catalog_json=\"/from-cli\""]
            ),
            "relative" => assert_eq!(
                result.unwrap_err(),
                "HAPI_CODEX_MODEL_CATALOG_JSON must be an absolute path (or start with ~/)."
            ),
            _ => assert_eq!(
                result.unwrap().server_args,
                ["-c", "model_catalog_json=\"/home/alice/catalog.json\""]
            ),
        }
        return;
    }
    for (case, value) in [
        ("home", "~//catalog.json"),
        ("relative", "relative"),
        ("override", "relative"),
    ] {
        assert!(std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "codex::launch::model_catalog_environment"])
            .env("HAPI_TEST_RUST_CATALOG_CASE", case)
            .env("HOME", "/home/alice")
            .env("HAPI_CODEX_MODEL_CATALOG_JSON", value)
            .status()
            .unwrap()
            .success());
    }
}
