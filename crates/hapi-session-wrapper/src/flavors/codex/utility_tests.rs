use super::*;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    io::{BufRead, BufReader, Write},
    process::{Command, Stdio},
};
#[test]
fn typescript_utility_suites() {
    let mut child = Command::new("bun")
        .args(["test", "src/flavors/codex/utility_parity.test.ts"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env("HAPI_RUST_CODEX_UTILITIES", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut clocks = HashMap::<i64, round_duration::RoundDuration>::new();
    let mut ledgers = HashMap::<i64, generation_ledger::GenerationLedger>::new();
    let mut scans = HashMap::<i64, compaction_summary::Lookup>::new();
    let mut count = 0;
    for line in BufReader::new(child.stdout.take().unwrap())
        .lines()
        .map_while(Result::ok)
    {
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        count += 1;
        let id = v[0].as_i64().unwrap();
        let method = v[1].as_str().unwrap();
        let a = &v[2];
        let b = &v[3];
        let c = &v[4];
        let s = a.as_str().unwrap_or("");
        let at = b.as_i64().unwrap_or(-1);
        let clock = clocks.entry(id).or_default();
        let ledger = ledgers.entry(id).or_default();
        let mut response = json!({});
        response["value"] =
            match method {
                "slash" => slash_commands::resolve(s, b),
                "matches" => json!(settings::matches(a, b)),
                "sandbox" => match settings::inherited_sandbox(a) {
                    Ok(v) => v,
                    Err(e) => {
                        response["error"] = json!(e);
                        Value::Null
                    }
                },
                "clockStart" => {
                    clock.start(s, at);
                    Value::Null
                }
                "finish" => json!(clock.finish(s, at, c == true)),
                "usage" => {
                    if let Some(n) = a.as_i64() {
                        ledger.usage(n);
                    }
                    Value::Null
                }
                "startTurn" => {
                    ledger.start(s, at);
                    Value::Null
                }
                "beginTool" => {
                    ledger.begin_tool(s, at);
                    Value::Null
                }
                "endTool" => {
                    ledger.end_tool(s, at, c.as_i64());
                    Value::Null
                }
                "beginUserWait" => json!(ledger.begin_wait(a.as_i64().unwrap())),
                "endUserWait" => {
                    ledger.end_wait(a.as_u64().unwrap(), at);
                    Value::Null
                }
                "reset" => {
                    ledger.reset();
                    Value::Null
                }
                "settle" => json!(ledger.settle(s, at, c == true)),
                "lookup" => {
                    scans.insert(id, compaction_summary::Lookup::new(s.into()));
                    Value::Null
                }
                "probe" => match scans.get_mut(&id).unwrap().probe(
                    a,
                    b.as_str().unwrap(),
                    c.as_str().unwrap(),
                ) {
                    Ok(v) => json!({"done":true,"result":v}),
                    Err(_) => json!({"done":false}),
                },
                _ => panic!("{method}"),
            };
        writeln!(input, "{response}").unwrap();
    }
    assert!(child.wait().unwrap().success());
    assert!(count > 100);
}
