use hapi_runner_rs::run_session_probe;
use std::env;

#[tokio::main]
async fn main() {
    let mut sessions = 1usize;
    let mut hold_ms = 0u64;
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--probe-sessions" => {
                if let Some(val) = args.next() {
                    sessions = val.parse::<usize>().unwrap_or(1).max(1);
                }
            }
            "--hold-ms" => {
                if let Some(val) = args.next() {
                    hold_ms = val.parse::<u64>().unwrap_or(0);
                }
            }
            _ => {}
        }
    }

    let probe = run_session_probe(sessions, hold_ms).await;
    println!(
        "{}",
        serde_json::to_string(&probe).expect("failed to serialize probe")
    );
}
