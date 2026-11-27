#![cfg(unix)]
use std::process::Command;

fn scenario(name: &str) {
    let output = Command::new("bun")
        .args([
            "src/flavors/claude/session_hub.ts",
            env!("CARGO_BIN_EXE_hapi-session-wrapper"),
            name,
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{name}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
#[test]
fn plain_turn() {
    scenario("plain");
}
#[test]
fn steer_and_live_mode() {
    scenario("mode");
}
#[test]
fn compact_clear_and_skill() {
    scenario("compact");
}
#[test]
fn respawn_cap_and_abort() {
    scenario("failure");
}
#[test]
fn fork_materializes_before_prompt() {
    scenario("fork");
}
#[test]
fn fork_from_an_earlier_message() {
    scenario("history");
}
#[test]
fn rewind_restarts_claude_and_waits_for_hub_cut() {
    scenario("rewind");
}
#[test]
fn pending_rewind_rearms_when_reopened() {
    scenario("rewind-reopen");
}
#[test]
fn pending_rewind_without_transcript_reopens() {
    scenario("rewind-reopen-no-transcript");
}
#[test]
fn pending_rewind_without_transcript_dispatches() {
    scenario("rewind-dispatch-no-transcript");
}
#[test]
fn fallback_after_create() {
    scenario("fallback");
}
#[test]
fn unsupported_and_version() {
    let call = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_hapi-session-wrapper"))
            .args(args)
            .env("HAPI_WRAPPER_CLI_VERSION", "test-version")
            .output()
            .unwrap()
    };
    let version = call(&["--version"]);
    assert!(version.status.success());
    assert!(String::from_utf8(version.stdout)
        .unwrap()
        .starts_with("test-version (rust "));
    for args in [
        &["unknown"][..],
        &["claude"],
        &["claude", "--started-by", "terminal"],
        &["claude", "--hapi-starting-mode", "local"],
    ] {
        let result = call(args);
        assert_eq!(result.status.code(), Some(86));
        if args[0] == "claude" {
            assert!(String::from_utf8_lossy(&result.stderr).contains("Local Claude sessions"));
        }
        assert!(!String::from_utf8_lossy(&result.stderr).contains("HAPI_RUST_WRAPPER_SESSION_ID="));
    }
}

#[test]
fn unacknowledged_steer_on_abort() {
    scenario("steer-abort");
}
#[test]
fn unacknowledged_steer_on_exit() {
    scenario("steer-close");
}
#[test]
fn effort_rejected() {
    scenario("effort-reject");
}
#[test]
fn effort_process_exits() {
    scenario("effort-exit");
}
#[test]
fn plan_and_failed_compaction() {
    scenario("plan");
}
#[test]
fn missing_environment_exits_before_webhook() {
    for missing in ["HAPI_WRAPPER_CLI_VERSION", "HAPI_WRAPPER_RUNTIME_PATH"] {
        let out = Command::new(env!("CARGO_BIN_EXE_hapi-session-wrapper"))
            .args(["claude", "--started-by", "runner"])
            .env("CLI_API_TOKEN", "fixture")
            .env(
                "HAPI_HOME",
                std::env::temp_dir().join("hapi-unused-wrapper-config"),
            )
            .env("HAPI_CLAUDE_PATH", "/bin/true")
            .env("HAPI_WRAPPER_CLI_VERSION", "fixture")
            .env("HAPI_WRAPPER_RUNTIME_PATH", "/tmp")
            .env_remove(missing)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(86));
        assert!(String::from_utf8_lossy(&out.stderr).contains(missing));
    }
}

#[test]
fn init_native_mode_rejection() {
    scenario("init-mode");
}
#[test]
fn stale_switch_rejection() {
    scenario("stale-mode");
}
#[test]
fn autonomous_stream_and_abort_during_compact_lookup() {
    scenario("compact-wait");
}

#[test]
fn queued_mode_precedes_prompt() {
    scenario("mode-order");
}

#[test]
fn resumed_first_compact() {
    scenario("resume");
}

#[test]
fn tool_permissions_and_sidechains() {
    scenario("tool");
}
#[test]
fn cleared_steer_on_abort() {
    scenario("steer-cleared");
}

#[test]
fn dropped_expanded_skill_echo() {
    scenario("skill-drop");
}
