use super::*;
use tempfile::tempdir;
fn write(path: &Path, body: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap()
}

#[cfg(unix)]
#[test]
fn command_scan_skips_symlink() {
    let dir = tempdir().unwrap();
    let target = dir.path().join("target.txt");
    write(&target, "linked command");
    std::os::unix::fs::symlink(&target, dir.path().join("linked.md")).unwrap();
    write(&dir.path().join("regular.md"), "regular command");

    assert_eq!(
        sorted_commands(dir.path(), "project", None),
        vec![
            json!({"name":"regular","description":"Custom command","source":"project","content":"regular command"})
        ]
    );
}

#[cfg(unix)]
#[test]
fn command_scan_skips_fifo_without_a_writer() {
    let dir = tempdir().unwrap();
    let fifo = dir.path().join("pipe.md");
    nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::S_IRWXU).unwrap();
    write(&dir.path().join("regular.md"), "regular command");

    let commands_dir = dir.path().to_owned();
    let (send, receive) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = send.send(sorted_commands(&commands_dir, "project", None));
    });
    let commands = receive
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("command scan blocked on a FIFO without a writer");
    assert_eq!(
        commands,
        vec![
            json!({"name":"regular","description":"Custom command","source":"project","content":"regular command"})
        ]
    );
}

fn commands_and_skills() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let child = root.join("sub");
    fs::create_dir(root.join(".git")).unwrap();
    for (f, subdir, expected) in [
        ("claude", ".claude/commands", "doctor"),
        ("codex", ".codex/prompts", "agent"),
        ("opencode", ".opencode/command", "init"),
        ("grok", ".grok/commands", "always-approve"),
    ] {
        write(&root.join(subdir).join("status.md"), "repo");
        write(
            &child.join(subdir).join("status.md"),
            "---\ndescription: near\n---\n\nbody",
        );
        write(&child.join(subdir).join("nested/start.md"), "nested");
        let cmds = commands(f, &child);
        assert!(cmds.iter().any(|v| v["name"] == expected));
        assert!(cmds.iter().any(|v| v["name"] == "nested:start"));
        let status: Vec<_> = cmds.iter().filter(|v| v["name"] == "status").collect();
        assert_eq!(
            status,
            vec![
                &json!({"name":"status","source":"project","description":"near","content":"body"})
            ]
        );
    }
    for (f, expected) in [
        ("agy", "swarm"),
        ("copilot", "fleet"),
        ("unknown", "doctor"),
    ] {
        assert!(commands(f, &child).iter().any(|v| v["name"] == expected))
    }
    for (f, subdir) in [
        ("codex", ".codex/skills/.system"),
        ("claude", ".claude/skills"),
        ("grok", ".grok/skills"),
        ("copilot", ".github/skills"),
        ("agy", "_agent/skills"),
        ("dsh", ".dsh/skills"),
    ] {
        let name = format!("test-skill-{f}");
        write(
            &child.join(subdir).join(&name).join("SKILL.md"),
            "# fallback",
        );
        assert!(skills(f, &child).iter().any(|v| v["name"] == name));
    }
    write(
        &root.join(".agents/skills/one/SKILL.md"),
        "---\nname: example\ndescription: far\n---\n",
    );
    write(
        &child.join(".agents/skills/one/SKILL.md"),
        "---\nname: example\ndescription: near\n---\n",
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        symlink(
            root.join(".agents/skills/one"),
            child.join(".agents/skills/link"),
        )
        .unwrap();
        let result = skills("claude", &child);
        assert_eq!(result.iter().filter(|v| v["name"] == "example").count(), 1);
        fs::remove_file(child.join(".agents/skills/link")).unwrap();
    }
    assert!(skills("claude", &child).contains(&json!({"name":"example","description":"near"})));
    fs::remove_dir(root.join(".git")).unwrap();
    assert_eq!(projects(&child), vec![child.as_path()]);
}
#[test]
fn installed_skill_and_command_excerpts() {
    let (skill, body) = frontmatter(
        r#"---
name: context7-mcp
description: This skill should be used when the user asks about libraries, frameworks, API references, or needs code examples. Activates for setup questions, code generation involving libraries, or mentions of specific frameworks like React, Vue, Next.js, Prisma, Supabase, etc.
---

When the user asks about libraries, frameworks, or needs code examples, use Context7 to fetch current documentation instead of relying on training data.
"#,
    );
    assert_eq!(skill["name"], "context7-mcp");
    assert!(field(&skill, "description").ends_with("Prisma, Supabase, etc."));
    assert!(body.starts_with("When the user asks"));
    let (command, body) = frontmatter(
        r#"---
description: '初始化 OpenSpec (OPSX) 环境 + 验证多模型 MCP 工具'
---
<!-- CCG:SPEC:INIT:START -->
**Core Philosophy**
- OPSX provides the specification framework; CCG adds multi-model collaboration.
- This phase ensures all tools are ready before any development work begins.
"#,
    );
    assert_eq!(
        command["description"],
        "初始化 OpenSpec (OPSX) 环境 + 验证多模型 MCP 工具"
    );
    assert!(body.starts_with("<!-- CCG:SPEC:INIT:START -->"));
}
#[test]
fn yaml12_string_fields_and_delimiters() {
    for (yaml, want) in [
        (
            "description: plain\n  continuation",
            json!({"description":"plain continuation"}),
        ),
        (
            "description: \"quoted\n  continuation\" # comment",
            json!({"description":"quoted continuation"}),
        ),
        (
            "description: 'quote' # comment\nname: inf",
            json!({"name":"inf","description":"quote"}),
        ),
        ("description: nan\nname: 23", json!({"description":"nan"})),
        ("name: [a,\n b]\ndescription: true", json!({})),
        (
            "description: >-\n  first\n  second",
            json!({"description":"first second"}),
        ),
        (
            "description: |\n  first\n  second",
            json!({"description":"first\nsecond\n"}),
        ),
    ] {
        assert_eq!(
            frontmatter(&format!("---\r\n{yaml}\r\n---\r\nbody")),
            (want, "body".into())
        );
    }
    for raw in [
        "---\ndescription: [unclosed\n---\nbody",
        "---\nname: first\nname: duplicate\n---\nbody",
    ] {
        assert_eq!(frontmatter(raw), (Value::Null, raw.into()));
    }
    assert_eq!(frontmatter("---\nname: skill\n---").0["name"], "skill");
}

#[test]
fn lookup_resolves_relative_project_and_flavor() {
    let cwd = std::env::current_dir().unwrap();
    let tmp = tempfile::tempdir_in(&cwd).unwrap();
    let root = tmp.path();
    fs::create_dir(root.join(".git")).unwrap();
    let skill = root.join(".claude/skills/review-round-one");
    fs::create_dir_all(&skill).unwrap();
    fs::write(skill.join("SKILL.md"), "skill body").unwrap();
    let child = root.join("nested");
    fs::create_dir(&child).unwrap();
    let relative = child.strip_prefix(&cwd).unwrap().join("unused/..");
    assert_eq!(
        resolve_skill(" Claude ", &relative, " review-round-one ").unwrap(),
        "Skill: review-round-one\n\nskill body"
    );
}

#[test]
fn lookup_validates_js_name_length() {
    for name in ["   ", &"😀".repeat(65), "../x"] {
        assert_eq!(
            resolve_skill("claude", Path::new("."), name).unwrap_err(),
            "Invalid skill name"
        );
    }
}
#[tokio::test]
async fn supplied_runtime_and_discovery_roots() {
    let marker = "HAPI_DISCOVERY_SANDBOX";
    if let Some(dir) = std::env::var_os(marker) {
        let base = PathBuf::from(dir);
        let root = base.join("project/child");
        commands_and_skills();
        let manager = RpcHandlerManager::new("s");
        register(&manager, &root);
        let rpc = |m: &str, v: Value| {
            let req = json!({"method":format!("s:{m}"),"params":v.to_string()});
            let manager = &manager;
            async move {
                serde_json::from_str::<Value>(&manager.handle_request(&req).await.unwrap()).unwrap()
            }
        };
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
        let comparison = std::process::Command::new("bun")
            .current_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../cli"))
            .args(["-e", r#"
                import { listSlashCommands } from './src/modules/common/slashCommands';
                import { listSkills, resolveSkill } from './src/modules/common/skills';
                const root = process.env.HAPI_DISCOVERY_SANDBOX + '/project/child';
                const result = {};
                for (const flavor of ['claude','codex','opencode','grok','copilot','agy','pi','cursor','gemini','kimi','dsh','unknown']) {
                    result[flavor] = [await listSlashCommands(flavor, root), await listSkills(root, {flavor})];
                }
                const selected = await resolveSkill("same-root", root);
                console.log(JSON.stringify([result, selected.body]));
            "#])
            .output().unwrap();
        assert!(
            comparison.status.success(),
            "{}",
            String::from_utf8_lossy(&comparison.stderr)
        );
        let (expected, selected_body): (BTreeMap<String, Value>, String) =
            serde_json::from_slice(&comparison.stdout).unwrap();
        assert!(resolve_skill("claude", &root, "same-root")
            .unwrap()
            .ends_with(&format!("\n\n{selected_body}")));
        for (flavor, values) in expected {
            assert_eq!(
                commands(&flavor, &root),
                values[0].as_array().unwrap().clone(),
                "{flavor}"
            );
            assert_eq!(
                skills(&flavor, &root),
                values[1].as_array().unwrap().clone(),
                "{flavor}"
            );
        }
        return;
    }
    let dir = tempdir().unwrap();
    let base = dir.path();
    let write = |p: &str, data: &str| {
        let p = base.join(p);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, data).unwrap()
    };
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
    fs::create_dir_all(base.join("project/child/nested")).unwrap();
    for sibling in ["z", "a", "b", "c"] {
        write(
            &format!("project/.agents/skills/{sibling}/SKILL.md"),
            &format!("---\nname: same-root\ndescription: {sibling}\n---\n{sibling}"),
        );
    }
    for (folder, scalar) in [
        ("null-upper", "NULL"),
        ("null-title", "Null"),
        ("null-quoted", r#""NULL""#),
    ] {
        write(
            &format!("project/.agents/skills/{folder}/SKILL.md"),
            &format!("---\nname: {scalar}\ndescription: {scalar}\n---\nbody"),
        );
    }
    write("project/.git", "gitdir: elsewhere");
    write("project/child/.claude/commands/local.md", "project");
    write(
        "config/commands/front.md",
        "---\ndescription: short\n---suffix\nbody",
    );
    write("config/commands/local.md", "user");
    write("old/commands/run.md", "old");
    write("new/commands/run.md", "new");
    write("config/plugins/installed_plugins.json",&json!({"plugins":{"p@market":[{"lastUpdated":"2026-01-02T03:00:00+05:00","installPath":base.join("old")},{"lastUpdated":"2026-01-01T24:00:00Z","installPath":base.join("new")}]}}).to_string());
    let result = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "rpc::handlers::discovery::tests_discovery::supplied_runtime_and_discovery_roots",
            "--nocapture",
        ])
        .env(marker, base)
        .env("HOME", base.join("home"))
        .env("USERPROFILE", base.join("home"))
        .env("XDG_CONFIG_HOME", base.join("xdg"))
        .env("CLAUDE_CONFIG_DIR", base.join("config"))
        .env("CODEX_HOME", base.join("codex"))
        .env("DSH_HOME", base.join("dsh"))
        .env("GROK_HOME", base.join("grok"))
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}

#[test]
fn frontmatter_accepts_scalar_aliases_within_budget() {
    let head = "label: &label Example\ndescription: *label";
    assert!(yaml_fields(head, 256).is_some());
    assert!(yaml_fields(head, 100).is_none());
    assert_eq!(
        frontmatter(&format!("---\n{head}\n---\nbody")).0["description"],
        "Example"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn invalid_utf8_directory_cannot_provide_a_skill_name() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let dir = tempdir().unwrap();
    let path = dir
        .path()
        .join(".agents/skills")
        .join(OsStr::from_bytes(&[0xff]))
        .join("SKILL.md");
    write(&path, "body");
    assert!(!skills("claude", dir.path())
        .iter()
        .any(|skill| skill["name"] == ""));
}

#[test]
fn skill_size_boundary() {
    let dir = tempdir().unwrap();
    let path = dir.path().join(".agents/skills/h3-boundary/SKILL.md");
    let body = "x".repeat(MAX_SKILL_BYTES);
    write(&path, &body);
    assert_eq!(
        resolve_skill("claude", dir.path(), "h3-boundary").unwrap(),
        format!("Skill: h3-boundary\n\n{body}")
    );
    write(&path, &(body + "x"));
    assert_eq!(
        resolve_skill("claude", dir.path(), "h3-boundary").unwrap_err(),
        "Skill is too large to load (maximum 131072 bytes)"
    );
}
