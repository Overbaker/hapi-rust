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
