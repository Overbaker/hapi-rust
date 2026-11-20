use super::{launch, registry, runtime, version};
use hapi_wrapper_core::{
    config::Config,
    lifecycle::{proctree, runner_lifecycle::startup_fallback, start_marker::alive},
    rest::Rest,
    session::metadata::invoked_cwd,
};
use serde_json::Value;
use std::{io, path::PathBuf, sync::Arc};

#[cfg(unix)]
fn canonical_home(home: &std::path::Path) -> io::Result<PathBuf> {
    home.canonicalize()
}

#[cfg(windows)]
fn canonical_home(home: &std::path::Path) -> io::Result<PathBuf> {
    use std::{os::windows::fs::MetadataExt, path::Component};
    let mut path = PathBuf::new();
    for component in std::path::absolute(home)?.components() {
        path.push(component);
        if let Component::Normal(_) = component {
            // Node's realpath preserves the spelling of ordinary components (including 8.3 aliases).
            if path.symlink_metadata()?.file_attributes() & 0x400 != 0 {
                path = display_home(path.canonicalize()?);
            }
        }
    }
    Ok(display_home(path))
}

#[cfg(windows)]
fn display_home(path: PathBuf) -> PathBuf {
    use std::path::{Component, Prefix};
    let mut parts = path.components();
    let Some(Component::Prefix(prefix)) = parts.next() else {
        return path;
    };
    let mut normalized = match prefix.kind() {
        Prefix::VerbatimDisk(drive) => PathBuf::from(format!("{}:\\", char::from(drive))),
        Prefix::VerbatimUNC(server, share) => PathBuf::from(r"\\").join(server).join(share),
        _ => return path,
    };
    normalized.extend(parts.filter(|part| matches!(part, Component::Normal(_))));
    normalized
}

pub async fn run(options: Value) -> i32 {
    let mut progress = runtime::Startup::default();
    let result = execute(options, &mut progress).await;

    match result {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("{error}");
            if progress.no_retry {
                1
            } else {
                startup_fallback(progress.session.as_deref())
            }
        }
    }
}

async fn execute(mut options: Value, progress: &mut runtime::Startup) -> io::Result<()> {
    let command = version::resolve().await.map_err(io::Error::other)?;
    if options["startedBy"] != "runner" {
        return Err(io::Error::other(
            "Native Codex terminal launch requires the TypeScript wrapper",
        ));
    }
    let config = Arc::new(Config::load().await?);
    let rest = Rest::new(&config).await?;
    progress.no_retry = true;
    if let Some(id) = options["existingSessionId"]
        .as_str()
        .filter(|_| options.get("reservedSessionId").is_none())
    {
        let row = rest.session(id).await?.session;
        if row["metadata"]["codexRuntime"]["kind"] == "attached" {
            return Err(io::Error::other(
                "Use hapi attach for this external thread; owned resume is disabled",
            ));
        }
        if registry::find_runtime(&config.home, &config.api_url, &config.cli_api_token, id)
            .await?
            .is_some()
        {
            return Err(io::Error::other(
                "Shared session is already running; attach instead of spawning another execution",
            ));
        }
        if row["active"] == true {
            return Err(io::Error::other("Existing session is active in another or legacy runtime. Stop it explicitly before cold resume; no hot migration."));
        }
        if options["resumeSessionId"].is_null() {
            options["resumeSessionId"] = row["metadata"]["codexSessionId"].clone();
        }
        if options["resumeSessionId"]
            .as_str()
            .is_none_or(str::is_empty)
        {
            return Err(io::Error::other(
                "Existing HAPI session has no Codex thread binding",
            ));
        }
    }
    if let Some(runner) = std::fs::read(config.home.join("runner.state.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
    {
        if runner["pid"]
            .as_u64()
            .and_then(|p| u32::try_from(p).ok())
            .is_some_and(alive)
            && runner["sharedCodexRuntime"] != true
        {
            return Err(io::Error::other("Running HAPI runner predates shared Codex sessions. Upgrade/restart the runner first; existing sessions are not migrated or stopped automatically."));
        }
    }
    let launch =
        launch::shared_launch_config(&options, &invoked_cwd()?).map_err(io::Error::other)?;
    let home = std::env::var_os("CODEX_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::home_dir().map(|p| p.join(".codex")))
        .ok_or_else(|| io::Error::other("Cannot determine CODEX_HOME"))?;
    registry::mkdir(&home)?;
    proctree::establish_wrapper_group()?;
    progress.no_retry = false;
    runtime::run(
        config,
        rest,
        command,
        canonical_home(&home)?,
        launch,
        options,
        progress,
    )
    .await
}
