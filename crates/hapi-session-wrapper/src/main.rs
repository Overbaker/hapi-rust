use hapi_session_wrapper::{parse, run};
use hapi_wrapper_core::lifecycle::runner_lifecycle::startup_fallback;

fn main() -> std::process::ExitCode {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|s| s == "--version") {
        let package: serde_json::Value =
            serde_json::from_str(include_str!("../../../cli/package.json")).unwrap();
        let version = std::env::var("HAPI_WRAPPER_CLI_VERSION")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| package["version"].as_str().unwrap().into());
        println!("{version} (rust {})", env!("HAPI_BUILD_REVISION"));
        return std::process::ExitCode::SUCCESS;
    }
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(4)
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("{e}");
            return std::process::ExitCode::from(startup_fallback(None) as u8);
        }
    };
    let code = runtime.block_on(async {
        match args.first().map(String::as_str) {
            Some("--parse-only") => match parse(&args[1..]) {
                Ok(v) => {
                    println!("{v}");
                    0
                }
                Err(e) => {
                    eprintln!("{e}");
                    1
                }
            },
            Some("mcp") => match hapi_wrapper_core::mcp::stdio_bridge::run(&args[1..]).await {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("[hapi-mcp] {e}");
                    2
                }
            },
            Some("hook-forwarder") => {
                match hapi_wrapper_core::hooks::forwarder::run(&args[1..]).await {
                    Ok(()) => 0,
                    Err(e) => {
                        eprintln!("{e}");
                        1
                    }
                }
            }
            _ => match parse(&args) {
                Ok(opts) => run(opts).await,
                Err(e) => {
                    eprintln!("{e}");
                    startup_fallback(None)
                }
            },
        }
    });
    std::process::ExitCode::from(code as u8)
}
