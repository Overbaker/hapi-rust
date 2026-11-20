use hapi_wrapper_core::lifecycle::runner_lifecycle::startup_fallback;
use serde_json::Value;

pub mod claude;
#[cfg(target_os = "linux")]
pub mod codex;
#[cfg(target_os = "linux")]
pub(crate) mod common;

pub fn parse(args: &[String]) -> Result<Value, String> {
    let Some((flavor, args)) = args.split_first() else {
        return Err("Missing flavor".into());
    };
    match flavor.as_str() {
        #[cfg(target_os = "linux")]
        "codex" => codex::parse(args),
        "claude" => claude::cli::parse(args),
        _ => Err(format!("Unsupported flavor: {flavor}")),
    }
}

pub async fn run(opts: Value) -> i32 {
    match opts["flavor"].as_str() {
        #[cfg(target_os = "linux")]
        Some("codex") => codex::frontend::run(opts).await,
        Some("claude") => claude::run::run(opts).await,
        _ => startup_fallback(None),
    }
}
