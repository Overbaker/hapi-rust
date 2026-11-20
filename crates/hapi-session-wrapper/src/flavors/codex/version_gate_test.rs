#![cfg(target_os = "linux")]
use std::{fs, os::unix::fs::PermissionsExt, process::Command};

#[test]
fn old_codex_exits_86_before_bootstrap() {
    let tmp = tempfile::tempdir().unwrap();
    let shim = tmp.path().join("codex");
    fs::write(
        &shim,
        "#!/bin/sh\n[ \"$#\" = 1 ] && [ \"$1\" = --version ] || exit 1\necho codex-cli 0.153.9\n",
    )
    .unwrap();
    fs::set_permissions(&shim, fs::Permissions::from_mode(0o700)).unwrap();
    let home = tmp.path().join("hapi");
    let result = Command::new(env!("CARGO_BIN_EXE_hapi-session-wrapper"))
        .arg("codex")
        .env("PATH", tmp.path())
        .env("HAPI_HOME", &home)
        .output()
        .unwrap();
    assert_eq!(result.status.code(), Some(86));
    assert_eq!(
        String::from_utf8(result.stderr).unwrap().trim(),
        "Shared sessions require Codex >= 0.154.0. Upgrade the Codex executable on PATH."
    );
    assert!(!home.exists());
}

#[test]
fn skips_empty_path_entries_and_checks_execute_access() {
    use std::os::unix::process::CommandExt;
    let tmp = tempfile::tempdir().unwrap();
    fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o755)).unwrap();
    for dir in ["first", "last"] {
        fs::create_dir(tmp.path().join(dir)).unwrap();
    }
    let make = |file: &str, mode, version: &str| {
        let p = tmp.path().join(file);
        fs::write(&p, format!("#!/bin/sh\necho codex-cli {version}\n")).unwrap();
        fs::set_permissions(p, fs::Permissions::from_mode(mode)).unwrap();
    };
    make("codex", 0o755, "1.0.0");
    let binary = tmp.path().join("wrapper");
    fs::copy(env!("CARGO_BIN_EXE_hapi-session-wrapper"), &binary).unwrap();
    let run = |path: &str| {
        let mut command = Command::new(&binary);
        if unsafe { libc::geteuid() } == 0 {
            command.gid(65534).uid(65534);
        }
        let out = command
            .current_dir(tmp.path())
            .arg("codex")
            .env("PATH", path)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(86));
        String::from_utf8(out.stderr).unwrap()
    };
    assert!(run(":").contains("Cannot find executable codex"));
    make(
        "first/codex",
        if unsafe { libc::geteuid() } == 0 {
            0o700
        } else {
            0o000
        },
        "0.153.0",
    );
    make("last/codex", 0o755, "1.0.0");
    assert!(run("first:last").contains("terminal launch requires the TypeScript wrapper"));
}
