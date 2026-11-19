use super::value::text;
use serde_json::{json, Value};
pub(super) fn normalize_model(v: &Value) -> Option<Value> {
    let s = |k: &str| v[k].as_str().map(str::trim).filter(|s| !s.is_empty());
    let id = s("id").or_else(|| s("model"))?;
    let mut out = json!({"id":id,"displayName":s("displayName").unwrap_or(id),"isDefault":v["isDefault"]==true,"defaultReasoningEffort":s("defaultReasoningEffort"),"defaultServiceTier":s("defaultServiceTier")});
    let efforts = v["supportedReasoningEfforts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| {
            text(e, "reasoningEffort")
                .map(str::trim)
                .filter(|s| !s.is_empty())
        })
        .collect::<Vec<_>>();
    if !efforts.is_empty() {
        out["supportedReasoningEfforts"] = json!(efforts);
    }
    let mut tiers = vec![];
    for entry in v["serviceTiers"].as_array().into_iter().flatten() {
        for k in ["id", "name"] {
            if let Some(s) = text(entry, k).map(str::trim).filter(|s| !s.is_empty()) {
                let s = s.to_lowercase();
                if !tiers.contains(&s) {
                    tiers.push(s);
                }
            }
        }
    }
    if !tiers.is_empty() {
        out["serviceTiers"] = json!(tiers);
    }
    Some(out)
}

use hapi_wrapper_core::{
    lifecycle::proctree::terminate_child_tree, rpc::manager::RpcHandlerManager,
};
use regex_lite::Regex;
use sha2::{Digest, Sha256};
use std::{
    io,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, LazyLock},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::Command,
    sync::Mutex,
    time::{timeout, Instant},
};

struct Cached {
    fingerprint: Vec<u8>,
    expires: Instant,
    models: Vec<Value>,
}
#[derive(Default)]
pub(super) struct Models {
    cache: [Mutex<Option<Cached>>; 2],
}
impl Models {
    pub fn register(self: &Arc<Self>, manager: &RpcHandlerManager, command: &Path, home: &Path) {
        let models = self.clone();
        let command = command.to_owned();
        let home = home.to_owned();
        manager.register("listCodexModels", move |p| {
            let models = models.clone();
            let command = command.clone();
            let home = home.clone();
            async move {
                Ok(Some(
                    match models
                        .list(&command, &home, p["includeHidden"] == true)
                        .await
                    {
                        Ok(v) => json!({"success":true,"models":v}),
                        Err(e) => json!({"success":false,"error":e.to_string()}),
                    },
                ))
            }
        });
    }
    async fn list(&self, command: &Path, home: &Path, hidden: bool) -> io::Result<Vec<Value>> {
        let mut cached = self.cache[hidden as usize].lock().await;
        let cwd = std::env::home_dir().ok_or_else(|| io::Error::other("No home directory"))?;
        let args = super::launch::shared_launch_config(&json!({}), &cwd)
            .map_err(io::Error::other)?
            .server_args;
        let fingerprint = fingerprint(home, &args).await?;
        if let Some(hit) = cached
            .as_ref()
            .filter(|v| v.fingerprint == fingerprint && v.expires > Instant::now())
        {
            return Ok(hit.models.clone());
        }
        let executable = std::env::var_os("HAPI_CODEX_APP_SERVER_BIN")
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| command.into());
        let result = probe(&executable, &cwd, &args, hidden).await?;
        if !result.is_empty() {
            *cached = Some(Cached {
                models: result.clone(),
                fingerprint,
                expires: Instant::now() + Duration::from_secs(86400),
            });
        }
        Ok(result)
    }
}
async fn optional(path: PathBuf) -> io::Result<Option<Vec<u8>>> {
    match tokio::fs::read(path).await {
        Ok(b) => Ok(Some(b)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}
async fn fingerprint(home: &Path, args: &[String]) -> io::Result<Vec<u8>> {
    static KEY: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"(?m)^\s*(?:model_catalog_json|"model_catalog_json"|'model_catalog_json')\s*=\s*("(?:[^"\\]|\\.)*"|'[^']*')"#).unwrap()
    });
    let (config, auth) = tokio::try_join!(
        optional(home.join("config.toml")),
        optional(home.join("auth.json"))
    )?;
    let root = String::from_utf8_lossy(config.as_deref().unwrap_or_default())
        .lines()
        .take_while(|line| !line.trim_start().starts_with('['))
        .collect::<Vec<_>>()
        .join("\n");
    let raw = args
        .get(1)
        .and_then(|s| s.strip_prefix("model_catalog_json="))
        .map(str::to_owned)
        .or_else(|| KEY.captures(&root).map(|c| c[1].to_string()));
    let path = raw
        .map(|s| {
            if s.starts_with('\'') {
                Ok(s[1..s.len() - 1].into())
            } else {
                serde_json::from_str::<String>(&s)
            }
        })
        .transpose()?;
    let catalog = if let Some(path) = &path {
        optional(home.join(path)).await?
    } else {
        None
    };
    let mut digest = Sha256::new();
    for bytes in [
        Some(home.as_os_str().as_encoded_bytes()),
        config.as_deref(),
        auth.as_deref(),
        path.as_deref().map(str::as_bytes),
        catalog.as_deref(),
    ] {
        digest.update(bytes.unwrap_or(b"missing"));
        digest.update([0]);
    }
    Ok(digest.finalize().to_vec())
}
struct ProbeProcess(Option<tokio::process::Child>);

fn stop_probe(mut child: tokio::process::Child) -> io::Result<tokio::process::Child> {
    if child.try_wait()?.is_none() {
        let stopped = terminate_child_tree(child.id().unwrap());
        if !matches!(stopped, Ok(true)) {
            return Err(io::Error::other(
                "Codex model discovery process did not stop",
            ));
        }
    }
    Ok(child)
}
impl Drop for ProbeProcess {
    fn drop(&mut self) {
        if let Some(child) = self.0.take() {
            if let Err(error) = stop_probe(child) {
                eprintln!("{error}");
            }
        }
    }
}
async fn probe(
    command: &Path,
    cwd: &Path,
    args: &[String],
    hidden: bool,
) -> io::Result<Vec<Value>> {
    let mut child = Command::new(command)
        .arg("app-server")
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let mut input = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let mut process = ProbeProcess(Some(child));
    let mut page = Value::Null;
    let result = async {
        for (id, method, p) in [(1, "initialize", json!({"clientInfo":{"name":"hapi-codex-models","version":"1.0.0"},"capabilities":{"experimentalApi":true}})), (2, "model/list", json!({"includeHidden":hidden}))] {
            timeout(Duration::from_secs(30), async {
                input.write_all(format!("{}\n", json!({"id":id,"method":method,"params":p})).as_bytes()).await?;
                loop {
                    let line = lines.next_line().await?.ok_or_else(|| io::Error::other("Codex model discovery disconnected"))?;
                    let v: Value = serde_json::from_str(&line)?;
                    if v["id"] != id || v.get("method").is_some() {
                        continue;
                    }
                    if let Some(error) = v.get("error") {
                        return Err(io::Error::other(text(error, "message").unwrap_or("Failed to list Codex models").to_string()));
                    }
                    page = v["result"].clone();
                    break;
                }
                Ok::<_, io::Error>(())
            })
            .await??;
            if id == 1 {
                input.write_all(b"{\"method\":\"initialized\"}\n").await?;
            }
        }
        Ok(page["data"].as_array().into_iter().flatten().filter_map(normalize_model).collect())
    }
    .await;
    drop(input);
    let child = process.0.take().unwrap();
    let mut child = tokio::task::spawn_blocking(move || stop_probe(child)).await??;
    child.wait().await?;
    result
}

#[cfg(test)]
#[tokio::test]
async fn common_model_handler_caches_by_account_catalog_and_visibility() {
    use std::process::Command;
    if std::env::var_os("HAPI_TEST_CODEX_MODELS").is_none() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::write(dir.join("config.toml"), "model_catalog_json='catalog'").unwrap();
        std::fs::write(dir.join("catalog"), "first").unwrap();
        assert!(Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "codex::models::common_model_handler_caches_by_account_catalog_and_visibility"
            ])
            .env("HAPI_TEST_CODEX_MODELS", dir)
            .env("ENGINE_LOG", dir.join("log"))
            .env_remove("HAPI_CODEX_MODEL_CATALOG_JSON")
            .env_remove("HAPI_CODEX_APP_SERVER_BIN")
            .status()
            .unwrap()
            .success());
        return;
    }
    let home = PathBuf::from(std::env::var_os("HAPI_TEST_CODEX_MODELS").unwrap());
    let rpc = RpcHandlerManager::new("row");
    Arc::new(Models::default()).register(
        &rpc,
        Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/flavors/codex/fake_engine.ts"
        )),
        &home,
    );
    let request = |hidden| {
        let q = json!({"method":"row:listCodexModels","params":json!({"includeHidden":hidden}).to_string()});
        let rpc = &rpc;
        async move {
            let r: Value = serde_json::from_str(&rpc.handle_request(&q).await.unwrap()).unwrap();
            assert_eq!(r["success"], true, "{r}");
            assert_eq!(r["models"][0]["id"], "mock");
        }
    };
    tokio::join!(request(false), request(false));
    request(true).await;
    std::fs::write(home.join("auth.json"), "different account").unwrap();
    request(true).await;
    std::fs::write(home.join("catalog"), "second").unwrap();
    request(true).await;
    assert_eq!(
        std::fs::read_to_string(home.join("log"))
            .unwrap()
            .lines()
            .filter(|line| line.contains("model/list"))
            .count(),
        4
    );
}
