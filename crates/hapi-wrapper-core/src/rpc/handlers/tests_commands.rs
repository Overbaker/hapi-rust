use super::super::tests::Fixture;
use super::*;
use std::{fs, os::unix::fs::symlink};
use tempfile::tempdir;
fn fixture() -> Fixture {
    let f = Fixture::new();
    register(&f.rpc, f.dir.path());
    f
}
fn git_in(dir: &Path, args: &[&str]) {
    assert!(std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .unwrap()
        .success());
}
#[tokio::test]
async fn shell_returns_stdout() {
    let f = fixture();
    assert_eq!(
        f.call("bash", json!({"command":"printf hello"})).await["stdout"],
        "hello"
    );
}
#[tokio::test]
async fn search_rejects_link_before_spawn() {
    let f = fixture();
    let outside = tempdir().unwrap();
    symlink(outside.path(), f.dir.path().join("linked")).unwrap();
    assert_eq!(
        f.call(
            "ripgrep",
            json!({"args":["--files","--","linked"],"fileSearch":{"query":"","limit":50}})
        )
        .await,
        error("Invalid file search path")
    );
}
#[tokio::test]
async fn git_status_and_diff() {
    let f = fixture();
    git_in(f.dir.path(), &["init", "-q"]);
    fs::write(f.dir.path().join("f"), "hello\n").unwrap();
    assert!(field(&f.call("git-status", json!({})).await, "stdout").contains("? f"));
    git_in(f.dir.path(), &["add", "f"]);
    assert_eq!(
        f.call("git-diff-numstat", json!({"staged":true})).await["stdout"],
        "1\t0\tf\n"
    );
    assert_eq!(f.call("git-diff-numstat", json!({})).await["stdout"], "");
    assert!(field(
        &f.call("git-diff-file", json!({"staged":true,"filePath":"f"}))
            .await,
        "stdout"
    )
    .contains("+hello"));
}
#[tokio::test]
async fn bash_errors_keep_output() {
    let f = fixture();
    let reply = f
        .call(
            "bash",
            json!({"command":"printf out; printf err >&2; exit 7"}),
        )
        .await;
    assert_eq!(reply["exitCode"], 7);
    assert_eq!(reply["stdout"], "out");
    assert_eq!(reply["stderr"], "err");
    assert_eq!(
        reply["error"],
        "Command failed: printf out; printf err >&2; exit 7\nerr"
    );
    let reply = f
        .call(
            "bash",
            json!({"command":"printf before; exec sleep 30","timeout":200}),
        )
        .await;
    assert_eq!(
        reply,
        json!({"success":false,"error":"Command timed out","stdout":"before","stderr":"","exitCode":-1})
    );
}
#[tokio::test]
async fn command_paths_and_limits() {
    let f = fixture();
    let root = f.dir.path();
    let other = tempdir().unwrap();
    symlink(other.path(), root.join("escape")).unwrap();
    fs::create_dir(root.join("sub")).unwrap();
    symlink("sub", root.join("alias")).unwrap();
    for m in [
        "bash",
        "git-status",
        "git-diff-numstat",
        "git-diff-file",
        "difftastic",
    ] {
        for (path, msg) in [
            (
                "../x",
                "Access denied: Path '../x' is outside the working directory",
            ),
            (
                "escape",
                "Access denied: resolved path is outside the working directory",
            ),
        ] {
            assert_eq!(
                f.call(m, json!({"cwd":path,"command":"pwd","args":[]}))
                    .await,
                error(msg),
                "{m}"
            );
        }
    }
    for path in ["../x", "escape"] {
        assert_eq!(
            f.call("ripgrep", json!({"cwd":path,"args":[]})).await,
            error("Invalid working directory")
        );
    }
    for (cwd, path) in [("", root.to_owned()), ("alias", root.join("sub"))] {
        let out = f.call("bash", json!({"cwd":cwd,"command":"pwd -P"})).await;
        assert_eq!(field(&out, "stdout").trim(), path.to_str().unwrap());
    }
    assert_eq!(
        field(&f.call("bash", json!({"command":"pwd -P"})).await, "stdout").trim(),
        root.to_str().unwrap()
    );
    git_in(root, &["init", "-q"]);
    for cwd in ["", "sub"] {
        assert_eq!(
            f.call("git-status", json!({"cwd":cwd})).await["success"],
            true
        )
    }
    for (path, msg) in [
        (
            "../x",
            "Access denied: Path '../x' is outside the working directory",
        ),
        (
            "escape/missing",
            "Access denied: resolved path is outside the working directory",
        ),
    ] {
        assert_eq!(
            f.call("git-diff-file", json!({"filePath":path})).await,
            error(msg)
        );
    }
    let out = f
        .call("bash", json!({"command":"head -c 1048577 /dev/zero"}))
        .await;
    assert_eq!(out["exitCode"], 1);
    assert_eq!(out["error"], "stdout maxBuffer length exceeded");
    assert_eq!(out["stderr"], "stdout maxBuffer length exceeded");
    assert_eq!(field(&out, "stdout").len(), 1 << 20);
}
#[test]
fn search_query_matching() {
    for (line, query, expected) in [
        ("src/One.RS", "", true),
        ("src/One.RS", "  one ", true),
        ("src/One.RS", "two", false),
        ("src/One.RS", "*.rs", true),
        ("src/One.RS", "src/?ne*", true),
        ("src/One.RS", "*.r", false),
        ("a/b/c", "a**c", true),
    ] {
        assert_eq!(search_match(line, query), expected, "{line} {query}");
    }
}
#[tokio::test]
async fn supplied_runtime_tools() {
    use std::os::unix::fs::PermissionsExt;
    let marker = "HAPI_COMMANDS_TEST_SANDBOX";
    if let Some(dir) = std::env::var_os(marker) {
        let root = PathBuf::from(dir).join("project");
        let manager = RpcHandlerManager::new("s");
        register(&manager, &root);
        let rpc = |m: &str, v: Value| {
            let req = json!({"method":format!("s:{m}"),"params":v.to_string()});
            let manager = &manager;
            async move {
                serde_json::from_str::<Value>(&manager.handle_request(&req).await.unwrap()).unwrap()
            }
        };
        let rg = rpc(
            "ripgrep",
            json!({"args":["--files"],"fileSearch":{"query":"*.rs","limit":1}}),
        )
        .await;
        assert_eq!(
            rg,
            json!({"success":true,"exitCode":0,"stdout":"src/one.RS\n","stderr":""})
        );
        for (limit, expected) in [
            (json!(-3), "src/one.RS\n"),
            (json!(1.9), "src/one.RS\nsecond.rs\n"),
            (json!(null), "src/one.RS\nsecond.rs\n"),
        ] {
            assert_eq!(
                rpc(
                    "ripgrep",
                    json!({"args":[],"fileSearch":{"limit":limit,"query":"*.rs"}})
                )
                .await["stdout"],
                expected
            );
        }
        let plain = rpc("ripgrep", json!({"args":["x"]})).await;
        assert_eq!(
            plain["stdout"], "first.txt\nsrc/one.RS\nsecond.rs\n",
            "{plain}"
        );
        for (cwd, path) in [("", root.clone()), ("nested", root.join("nested"))] {
            let diff = rpc("difftastic", json!({"cwd":cwd,"args":["pwd"]})).await;
            assert_eq!(field(&diff, "stdout").trim(), path.to_str().unwrap());
        }
        let diff = rpc("difftastic", json!({"args":["one","two words"]})).await;
        assert_eq!(
            diff,
            json!({"success":true,"exitCode":2,"stdout":"1|one|two words","stderr":""})
        );
        symlink("nested", root.join("alias")).unwrap();
        let rg = rpc(
            "ripgrep",
            json!({"args":["paths","--","alias"],"fileSearch":{"query":"","limit":2}}),
        )
        .await;
        assert_eq!(rg["stdout"], "nested\n");
        return;
    }
    let dir = tempdir().unwrap();
    let base = dir.path();
    let script = |p: &str, data: &str| {
        let p = base.join(p);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, data).unwrap();
        fs::set_permissions(p, fs::Permissions::from_mode(0o700)).unwrap()
    };
    script(
        "runtime/tools/unpacked/rg",
        "#!/bin/sh\n[ \"$(readlink /proc/self/fd/0)\" = /dev/null ] || exit 3\nif [ \"$1\" = paths ]; then echo \"$3\"; exit; fi\nprintf 'first.txt\\nsrc/one.RS\\nsecond.rs\\n'\n",
    );
    script(
        "runtime/tools/unpacked/difft",
        "#!/bin/sh\nif [ \"$1\" = pwd ]; then pwd -P; exit; fi\nprintf '%s|%s|%s' \"$FORCE_COLOR\" \"$1\" \"$2\"\nexit 2\n",
    );
    fs::create_dir_all(base.join("project/nested")).unwrap();
    let result = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "rpc::handlers::commands::tests::supplied_runtime_tools",
            "--nocapture",
        ])
        .env(marker, base)
        .env("HAPI_WRAPPER_RUNTIME_PATH", base.join("runtime"))
        .stdin(std::process::Stdio::piped())
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(String::from_utf8_lossy(&result.stdout).contains("1 passed"));
}
