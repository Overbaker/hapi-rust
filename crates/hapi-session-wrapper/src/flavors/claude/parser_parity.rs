#![cfg(unix)]
use serde_json::Value;
use std::process::Command;

#[test]
fn differential() {
    let output = Command::new("bun")
        .arg("src/flavors/claude/parser_cases.ts")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let cases: Vec<(Vec<String>, Value)> = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(cases.len(), 512);
    for (argv, expected) in cases {
        let actual = Command::new(env!("CARGO_BIN_EXE_hapi-session-wrapper"))
            .arg("--parse-only")
            .args(&argv)
            .output()
            .unwrap();
        assert!(
            actual.status.success(),
            "{argv:?}: {}",
            String::from_utf8_lossy(&actual.stderr)
        );
        assert_eq!(
            serde_json::from_slice::<Value>(&actual.stdout).unwrap(),
            expected,
            "{argv:?}"
        );
    }
}

#[test]
fn invalid_values_and_explicit_permission_precedence() {
    let parse = |args: &[&str]| {
        hapi_session_wrapper::parse(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    };
    assert!(parse(&["claude", "--model"]).is_err());
    assert!(parse(&["claude", "--permission-mode", "bad"]).is_err());
    assert_eq!(
        parse(&["claude", "--permission-mode", "plan", "--yolo"]).unwrap()["permissionMode"],
        "plan"
    );
}
