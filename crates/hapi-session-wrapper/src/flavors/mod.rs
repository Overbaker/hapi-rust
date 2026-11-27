use hapi_wrapper_core::lifecycle::startup_fallback;
use serde_json::Value;

#[cfg(any(target_os = "linux", windows))]
pub mod agy;
#[cfg(target_os = "linux")]
pub mod claude;
#[cfg(all(unix, not(target_os = "linux")))]
pub mod claude {
    pub mod cli;
}
#[cfg(any(target_os = "linux", windows))]
pub mod codex;
#[cfg(any(target_os = "linux", windows))]
pub(crate) mod common;

#[cfg(any(target_os = "linux", windows))]
pub mod pi;

pub fn parse(args: &[String]) -> Result<Value, String> {
    let Some((flavor, args)) = args.split_first() else {
        return Err("Missing flavor".into());
    };
    match flavor.as_str() {
        #[cfg(any(target_os = "linux", windows))]
        "pi" => pi::parse(args),
        #[cfg(any(target_os = "linux", windows))]
        "agy" => agy::parse(args),
        #[cfg(any(target_os = "linux", windows))]
        "codex" => codex::parse(args),
        #[cfg(unix)]
        "claude" => claude::cli::parse(args),
        _ => Err(format!("Unsupported flavor: {flavor}")),
    }
}

pub async fn run(opts: Value) -> i32 {
    match opts["flavor"].as_str() {
        #[cfg(any(target_os = "linux", windows))]
        Some("pi") => pi::run(opts).await,
        #[cfg(any(target_os = "linux", windows))]
        Some("agy") => agy::run::run(opts).await,
        #[cfg(any(target_os = "linux", windows))]
        Some("codex") => codex::frontend::run(opts).await,
        #[cfg(target_os = "linux")]
        Some("claude") => claude::run::run(opts).await,
        _ => startup_fallback(None),
    }
}
