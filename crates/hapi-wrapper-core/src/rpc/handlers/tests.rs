use super::*;
use std::{fs, os::unix::fs::symlink};
use tempfile::tempdir;
struct Fixture {
    dir: tempfile::TempDir,
    rpc: RpcHandlerManager,
    images: GeneratedImages,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempdir().unwrap();
        let rpc = RpcHandlerManager::new("s");
        let images = register_common_handlers(&rpc, dir.path());
        Self { dir, rpc, images }
    }
    async fn call(&self, m: &str, v: Value) -> Value {
        serde_json::from_str(
            &self
                .rpc
                .handle_request(&json!({"method":format!("s:{m}"),"params":v.to_string()}))
                .await
                .unwrap(),
        )
        .unwrap()
    }
}
#[test]
fn lexical_boundaries() {
    let root = Path::new("/home/user/project");
    for raw in [
        "/home/user/project/file.txt",
        "file.txt",
        "./src/file.txt",
        ".",
        "/home/user/project",
    ] {
        assert!(lexical(root, raw).is_ok())
    }
    for raw in [
        "/etc/passwd",
        "../../.ssh/id_rsa",
        "/home/user/project2/file.txt",
    ] {
        assert!(lexical(root, raw).is_err())
    }
    assert!(lexical(Path::new("/"), "/etc/passwd").is_ok());
}
#[tokio::test]
async fn bounded_file_reads() {
    let f = Fixture::new();
    let dir = f.dir.path();
    let outside = tempdir().unwrap();
    fs::write(dir.join("README.md"), "# test").unwrap();
    let reply = f.call("readFile", json!({"path":"README.md"})).await;
    assert_eq!(reply["size"], 6);
    assert_eq!(reply["content"], "IyB0ZXN0");
    assert!(reply["modified"].is_number());
    fs::File::create(dir.join("sparse"))
        .unwrap()
        .set_len(1000000000)
        .unwrap();
    fs::create_dir(dir.join("sub")).unwrap();
    symlink(dir.join("sub"), dir.join("link")).unwrap();
    fs::write(outside.path().join("secret"), "secret").unwrap();
    symlink(outside.path().join("secret"), dir.join("secret")).unwrap();
    for (path, max, msg) in [
        ("README.md", 1, "too large"),
        ("sparse", 16, "too large"),
        ("sub", 16, "regular file"),
        ("link", 16, "regular file"),
        ("secret", 16, "outside"),
    ] {
        let v = f
            .call("readFile", json!({"path":path,"maxBytes":max}))
            .await;
        assert!(field(&v, "error").contains(msg));
    }
}
#[tokio::test]
async fn directories_and_stat() {
    let f = Fixture::new();
    let dir = f.dir.path();
    let outside = tempdir().unwrap();
    fs::create_dir(dir.join("src")).unwrap();
    fs::write(dir.join("README.md"), "# test").unwrap();
    symlink(outside.path(), dir.join("linked")).unwrap();
    symlink("absent", dir.join("bad-link")).unwrap();
    let v = f.call("listDirectory", json!({"path":""})).await;
    let entries = v["entries"].as_array().unwrap();
    assert_eq!(entries[0]["name"], "src");
    assert!(entries.contains(&json!({"name":"bad-link","type":"other"})));
    let v = f
        .call(
            "statFiles",
            json!({"paths":["README.md","src","missing","linked"]}),
        )
        .await;
    assert_eq!(v["entries"][0]["size"], 6);
    assert_eq!(v["entries"][1]["type"], "directory");
    assert_eq!(v["entries"][2], json!({"path":"missing"}));
    assert_eq!(v["entries"][3], json!({"path":"linked"}));
    assert_eq!(
        f.call("statFiles", json!({"paths":["../no"]})).await["success"],
        false
    );
    assert_eq!(
        f.call("getDirectoryTree", json!({"path":".","maxDepth":1}))
            .await["tree"]["children"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}
#[tokio::test]
async fn writes_images_and_shell() {
    let f = Fixture::new();
    let req = json!({"path":"file","content":"YQ"});
    let v = f.call("writeFile", req.clone()).await;
    assert!(v["hash"].is_string());
    assert_eq!(
        f.call("writeFile", req).await["error"],
        "File already exists but was expected to be new"
    );
    assert_eq!(
        f.call(
            "writeFile",
            json!({"path":"file","content":"Yg","expectedHash":v["hash"]})
        )
        .await["success"],
        true
    );
    f.images
        .insert("i".into(), vec![1], "image/png".into(), "i.png".into())
        .unwrap();
    assert_eq!(
        f.call("readGeneratedImage", json!({"id":"i"})).await["content"],
        "AQ=="
    );
    assert_eq!(
        f.call("bash", json!({"command":"printf hello"})).await["stdout"],
        "hello"
    );
}
#[tokio::test]
async fn search_rejects_link_before_spawn() {
    let f = Fixture::new();
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
    let f = Fixture::new();
    assert!(std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(f.dir.path())
        .status()
        .unwrap()
        .success());
    fs::write(f.dir.path().join("f"), "hello\n").unwrap();
    assert!(field(&f.call("git-status", json!({})).await, "stdout").contains("? f"));
    assert!(std::process::Command::new("git")
        .args(["add", "f"])
        .current_dir(f.dir.path())
        .status()
        .unwrap()
        .success());
    assert_eq!(
        f.call("git-diff-numstat", json!({"staged":true})).await["stdout"],
        "1\t0\tf\n"
    );
    assert!(field(
        &f.call("git-diff-file", json!({"staged":true,"filePath":"f"}))
            .await,
        "stdout"
    )
    .contains("+hello"));
}
#[tokio::test]
async fn supplied_runtime_and_discovery_roots() {
    use std::os::unix::fs::PermissionsExt;
    let marker = "HAPI_HANDLER_TEST_SANDBOX";
    if let Some(dir) = std::env::var_os(marker) {
        let base = PathBuf::from(dir);
        let root = base.join("project/child");
        let manager = RpcHandlerManager::new("s");
        register_common_handlers(&manager, &root);
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
        assert_eq!(rg["stdout"], "src/one.RS\n");
        assert_eq!(rg["success"], true);
        let diff = rpc("difftastic", json!({"args":["one","two words"]})).await;
        assert_eq!(
            diff,
            json!({"success":true,"exitCode":2,"stdout":"1|one|two words","stderr":""})
        );
        let cmds = rpc("listSlashCommands", json!({"agent":"claude"})).await;
        let cmds = cmds["commands"].as_array().unwrap();
        assert!(cmds
            .iter()
            .any(|c| c["name"] == "p:run" && c["content"] == "new"));
        assert!(cmds
            .iter()
            .any(|c| c["name"] == "local" && c["source"] == "project"));
        for (f, expected) in [
            ("claude", "claude"),
            ("codex", "system"),
            ("dsh", "dsh"),
            ("grok", "grok"),
            ("copilot", "copilot"),
            ("agy", "agy"),
        ] {
            let reply = rpc("listSkills", json!({"flavor":f})).await;
            let skills = reply["skills"].as_array().unwrap();
            assert!(skills.contains(&json!({"name":expected})));
            assert!(skills.contains(&json!({"name":"shared","description":"project"})));
            if f == "claude" {
                assert!(skills.contains(&json!({"name":"plugin"})));
                assert!(!skills.contains(&json!({"name":"stale"})))
            }
        }
        assert_eq!(
            rpc("uploadInit", json!({"filename":"large","size":(3<<20)})).await["error"],
            "File too large (max 2MB)"
        );
        let model = rpc("listClaudeModels", json!({})).await;
        assert_eq!(model, error("Not logged in"));
        assert_eq!(
            rpc("listCodexModels", json!({})).await,
            json!({"error":"Method not found"})
        );
        return;
    }
    let dir = tempdir().unwrap();
    let base = dir.path();
    let write = |p: &str, data: &str| {
        let p = base.join(p);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, data).unwrap()
    };
    let script = |p: &str, data: &str| {
        write(p, data);
        fs::set_permissions(base.join(p), fs::Permissions::from_mode(0o700)).unwrap()
    };
    script(
        "runtime/tools/unpacked/rg",
        "#!/bin/sh\nprintf 'first.txt\\nsrc/one.RS\\nsecond.rs\\n'\n",
    );
    script(
        "runtime/tools/unpacked/difft",
        "#!/bin/sh\nprintf '%s|%s|%s' \"$FORCE_COLOR\" \"$1\" \"$2\"\nexit 2\n",
    );
    script("claude", "#!/bin/sh\necho 'Not logged in' >&2\nexit 1\n");
    for path in [
        "config/skills/claude",
        "codex/skills/.system/system",
        "dsh/skills/dsh",
        "grok/skills/grok",
        "home/.copilot/skills/copilot",
        "home/.gemini/antigravity-cli/skills/agy",
        "new/skills/plugin",
        "old/skills/stale",
    ] {
        write(&format!("{path}/SKILL.md"), "body")
    }
    write(
        "home/.agents/skills/shared/SKILL.md",
        "---\ndescription: global\n---\n",
    );
    write(
        "project/.agents/skills/shared/SKILL.md",
        "---\ndescription: project\n---\n",
    );
    write("project/.git", "gitdir: elsewhere");
    write("project/child/.claude/commands/local.md", "project");
    write("config/commands/local.md", "user");
    write("old/commands/run.md", "old");
    write("new/commands/run.md", "new");
    write("config/plugins/installed_plugins.json",&json!({"plugins":{"p@market":[{"lastUpdated":"2026-01-01","installPath":base.join("old")},{"lastUpdated":"2026-01-02","installPath":base.join("new")}]}}).to_string());
    let result = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "rpc::handlers::tests::supplied_runtime_and_discovery_roots",
            "--nocapture",
        ])
        .env(marker, base)
        .env("HOME", base.join("home"))
        .env("CLAUDE_CONFIG_DIR", base.join("config"))
        .env("CODEX_HOME", base.join("codex"))
        .env("DSH_HOME", base.join("dsh"))
        .env("GROK_HOME", base.join("grok"))
        .env("HAPI_WRAPPER_RUNTIME_PATH", base.join("runtime"))
        .env("HAPI_CLAUDE_PATH", base.join("claude"))
        .env("HAPI_MAX_UPLOAD_BYTES", "2097152")
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}
#[tokio::test]
async fn bash_errors_keep_output() {
    let f = Fixture::new();
    let reply = f
        .call(
            "bash",
            json!({"command":"printf out; printf err >&2; exit 7"}),
        )
        .await;
    assert_eq!(reply["exitCode"], 7);
    assert_eq!(reply["stdout"], "out");
    assert_eq!(reply["stderr"], "err");
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
