use hapi_wrapper_core::lifecycle::runner_lifecycle::startup_fallback;
use serde_json::Value;

pub mod claude;

pub fn parse(args: &[String]) -> Result<Value, String> {
    let Some((flavor, args)) = args.split_first() else {
        return Err("Missing flavor".into());
    };
    match flavor.as_str() {
        "claude" => claude::cli::parse(args),
        _ => Err(format!("Unsupported flavor: {flavor}")),
    }
}

pub async fn run(opts: Value) -> i32 {
    match opts["flavor"].as_str() {
        Some("claude") => claude::run::run(opts).await,
        _ => startup_fallback(None),
    }
}
