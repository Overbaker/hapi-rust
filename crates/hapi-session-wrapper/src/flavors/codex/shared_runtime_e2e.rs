#![cfg(any(target_os = "linux", windows))]
use std::process::Command;
fn scenario(name: &str) {
    let out = Command::new("bun")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .args([
            "src/flavors/codex/runtime_hub.ts",
            env!("CARGO_BIN_EXE_hapi-session-wrapper"),
            name,
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{name}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
#[cfg(target_os = "linux")]
#[test]
fn shared_turn_approval_new_and_stop() {
    scenario("shared")
}
#[cfg(target_os = "linux")]
#[test]
fn cold_resume_unarchives_and_signals_suspend() {
    scenario("cold")
}
#[cfg(target_os = "linux")]
#[test]
fn gateway_fork_and_interrupt() {
    scenario("gateway")
}
#[cfg(target_os = "linux")]
#[test]
fn termination_suspends() {
    scenario("terminate");
    scenario("webhook-error")
}
#[cfg(target_os = "linux")]
#[test]
fn frontend_rejects_before_bootstrap() {
    for case in [
        "attached",
        "legacy-active",
        "missing-binding",
        "old-runner",
        "owner-conflict",
    ] {
        scenario(case);
    }
}
#[cfg(target_os = "linux")]
#[test]
fn cold_resume_only_retries_an_archived_rejection() {
    for case in ["other-rejection", "transport-loss"] {
        scenario(case);
    }
}

#[cfg(target_os = "linux")]
#[test]
fn stdio_catalog_matches_typescript_and_forwards_streamable_http() {
    let out = Command::new("bun")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .args([
            "src/flavors/codex/mcp_parity.ts",
            env!("CARGO_BIN_EXE_hapi-session-wrapper"),
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[cfg(target_os = "linux")]
#[test]
fn startup_interrupt_and_engine_death() {
    for case in [
        "signal-marker",
        "signal-bootstrap",
        "signal-start",
        "init-exit",
        "live-exit",
    ] {
        scenario(case)
    }
}
#[cfg(target_os = "linux")]
#[test]
fn fallback_requires_safe_takeover() {
    for case in [
        "prepare-error",
        "fresh-prepare-error",
        "reserved-prepare-error",
        "start-rejection",
        "unreserved-rejection",
        "start-loss",
    ] {
        scenario(case)
    }
}
#[cfg(target_os = "linux")]
#[test]
fn failed_reservations_and_disk_rollback() {
    for case in ["last-rejection", "rollback-cold", "rollback-create"] {
        scenario(case)
    }
}

#[cfg(windows)]
#[test]
fn authenticated_loopback_turn_and_stop() {
    scenario("windows");
}
