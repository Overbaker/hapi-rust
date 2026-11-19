use super::*;
use std::{collections::BTreeMap, env, fs, io::Read, sync::LazyLock};
fn home() -> PathBuf {
    env::home_dir().unwrap_or_else(|| PathBuf::from("/"))
}
fn configured(k: &str, fallback: PathBuf) -> PathBuf {
    env::var_os(k)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or(fallback)
}
fn config(f: &str) -> PathBuf {
    let default = home().join(format!(".{f}"));
    match f {
        "claude" => configured("CLAUDE_CONFIG_DIR", default),
        "codex" => configured("CODEX_HOME", default),
        "grok" => configured("GROK_HOME", default),
        "dsh" => configured("DSH_HOME", default),
        "agy" => home().join(".gemini/config"),
        _ => default,
    }
}
fn projects(root: &Path) -> Vec<&Path> {
    let mut dirs = vec![];
    for p in root.ancestors() {
        dirs.push(p);
        if p.join(".git").exists() {
            return dirs;
        }
    }
    vec![root]
}
fn plugins(f: &str) -> Vec<(String, PathBuf)> {
    let data = fs::read(config(f).join("plugins/installed_plugins.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .unwrap_or_default();
    data["plugins"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(name, installs)| {
            let latest = installs.as_array()?.iter().rev().max_by_key(|i| {
                i["lastUpdated"]
                    .as_str()
                    .or(i["installedAt"].as_str())
                    .unwrap_or("")
            })?;
            let path = latest["installPath"].as_str().filter(|p| !p.is_empty())?;
            let name = name
                .rsplit_once('@')
                .filter(|(p, _)| !p.is_empty())
                .map(|(p, _)| p)
                .unwrap_or(name);
            Some((name.into(), PathBuf::from(path)))
        })
        .collect()
}
fn frontmatter(s: &str) -> (Value, String) {
    let fallback = || (Value::Null, s.trim().to_owned());
    let Some(rest) = s
        .strip_prefix("---\n")
        .or_else(|| s.strip_prefix("---\r\n"))
    else {
        return fallback();
    };
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end_matches(['\r', '\n']) == "---" {
            let head = &rest[..offset];
            let Ok(docs) = yaml_rust2::YamlLoader::load_from_str(
                head.strip_suffix("\r\n")
                    .or_else(|| head.strip_suffix('\n'))
                    .unwrap_or(head),
            ) else {
                return fallback();
            };
            if docs.len() > 1 {
                return fallback();
            }
            let mut fields = json!({});
            if let Some(doc) = docs.first() {
                for k in ["name", "description"] {
                    if let Some(value) = doc[k].as_str() {
                        fields[k] = json!(value)
                    }
                }
            }
            return (fields, rest[offset + line.len()..].trim().to_owned());
        }
        offset += line.len();
    }
    fallback()
}
fn dir_entries(dir: &Path) -> Vec<fs::DirEntry> {
    let mut entries: Vec<_> = fs::read_dir(dir).into_iter().flatten().flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    entries
}
fn scan_commands(
    dir: &Path,
    prefix: &str,
    source: &str,
    plugin: Option<&str>,
    out: &mut Vec<Value>,
) {
    for entry in dir_entries(dir) {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || name.contains(':') {
            continue;
        }
        let Ok(t) = entry.file_type() else { continue };
        if t.is_dir() {
            scan_commands(
                &entry.path(),
                &format!("{prefix}{name}:"),
                source,
                plugin,
                out,
            );
            continue;
        }
        let Some(stem) = name.strip_suffix(".md").filter(|_| t.is_file()) else {
            continue;
        };
        let mut v = json!({"name":format!("{prefix}{stem}"),"description":plugin.map(|p|format!("{p} command")).unwrap_or_else(||"Custom command".into()),"source":source});
        if let Some(p) = plugin {
            v["pluginName"] = json!(p)
        }
        if let Ok(content) = fs::read(entry.path()) {
            let (fields, body) = frontmatter(&String::from_utf8_lossy(&content));
            v["content"] = json!(body);
            if let Some(d) = fields.get("description") {
                v["description"] = d.clone()
            }
        }
        out.push(v)
    }
}
fn sorted_commands(dir: &Path, source: &str, plugin: Option<&str>) -> Vec<Value> {
    let mut out = vec![];
    scan_commands(
        dir,
        &plugin.map(|p| format!("{p}:")).unwrap_or_default(),
        source,
        plugin,
        &mut out,
    );
    out.sort_by(|a, b| compare_names(field(a, "name"), field(b, "name")));
    out
}
static BUILTINS: LazyLock<BTreeMap<String, Vec<Value>>> = LazyLock::new(|| {
    include_str!("builtins.jsonl")
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect()
});
fn commands(f: &str, root: &Path) -> Vec<Value> {
    let mut out = BUILTINS.get(f).unwrap_or(&BUILTINS["claude"]).clone();
    let (global, local) = match f {
        "claude" => (config(f).join("commands"), ".claude/commands"),
        "codex" => (config(f).join("prompts"), ".codex/prompts"),
        "opencode" => (
            configured("XDG_CONFIG_HOME", home().join(".config")).join("opencode/command"),
            ".opencode/command",
        ),
        "grok" => (home().join(".agents/commands"), ".grok/commands"),
        _ => return out,
    };
    out.extend(sorted_commands(&global, "user", None));
    if f == "claude" {
        let mut installed = vec![];
        for (name, p) in plugins(f) {
            installed.extend(sorted_commands(&p.join("commands"), "plugin", Some(&name)))
        }
        installed.sort_by(|a, b| compare_names(field(a, "name"), field(b, "name")));
        out.extend(installed)
    }
    for p in projects(root).into_iter().rev() {
        out.extend(sorted_commands(&p.join(local), "project", None))
    }
    let mut seen = std::collections::HashSet::new();
    out.reverse();
    out.retain(|v| seen.insert(field(v, "name").to_lowercase()));
    out.reverse();
    out
}
fn skill_dirs(root: &Path, system: bool) -> Vec<PathBuf> {
    let mut dirs = vec![];
    for entry in dir_entries(root) {
        let p = entry.path();
        if !p.is_dir() {
            continue;
        }
        let name = entry.file_name();
        if system && name == ".system" {
            dirs.extend(skill_dirs(&p, false))
        } else if !name.to_string_lossy().starts_with('.') {
            dirs.push(p)
        }
    }
    dirs
}
fn discover_skills(f: &str, root: &Path, lookup: Option<&str>) -> Vec<Value> {
    let mut roots = vec![];
    for p in projects(root) {
        roots.push(p.join(".agents/skills"));
        match f {
            "claude" | "codex" | "grok" | "copilot" | "dsh" => {
                roots.push(p.join(format!(".{f}/skills")))
            }
            "agy" => roots
                .extend([".agent/skills", "_agents/skills", "_agent/skills"].map(|s| p.join(s))),
            _ => {}
        }
        if f == "copilot" {
            roots.push(p.join(".github/skills"))
        }
    }
    roots.push(home().join(".agents/skills"));
    if matches!(f, "claude" | "codex" | "grok" | "copilot" | "dsh" | "agy") {
        roots.push(config(f).join("skills"))
    }
    if f == "agy" {
        roots.push(home().join(".gemini/antigravity-cli/skills"))
    }
    let mut dirs = vec![];
    for root in roots {
        dirs.extend(skill_dirs(
            &root,
            f == "codex" && (root.ends_with(".codex/skills") || root == config(f).join("skills")),
        ))
    }
    for (_, plugin) in plugins(f) {
        dirs.extend(skill_dirs(&plugin.join("skills"), false))
    }
    if f == "codex" {
        dirs.extend(skill_dirs(Path::new("/etc/codex/skills"), true))
    }
    let mut found = BTreeMap::new();
    for p in dirs {
        let Ok(file) = fs::File::open(p.join("SKILL.md")) else {
            continue;
        };
        if !file.metadata().is_ok_and(|m| m.is_file()) {
            continue;
        }
        let mut bytes = vec![];
        if file.take(128 * 1024 + 1).read_to_end(&mut bytes).is_err() {
            continue;
        }
        let (fields, body) = frontmatter(&String::from_utf8_lossy(&bytes));
        let mut name = field(&fields, "name").trim();
        if name.is_empty() {
            name = p.file_name().and_then(|s| s.to_str()).unwrap_or("")
        }
        if let Some(target) = lookup {
            if target == name {
                return vec![
                    json!({"name":name,"description":fields["description"],"body":body,"oversized":bytes.len()>128*1024}),
                ];
            }
            continue;
        }
        found.entry(name.to_owned()).or_insert_with(|| {
            let mut v = json!({"name":name});
            if let Some(d) = fields["description"].as_str() {
                v["description"] = json!(d.trim())
            }
            v
        });
    }
    let mut out: Vec<_> = found.into_values().collect();
    out.sort_by(|a, b| compare_names(field(a, "name"), field(b, "name")));
    out
}
fn skills(f: &str, root: &Path) -> Vec<Value> {
    discover_skills(f, root, None)
}

pub(crate) fn resolve_skill(f: &str, root: &Path, name: &str) -> Result<String, String> {
    if name.is_empty() || matches!(name, "." | "..") || name.contains(['/', '\\', '\0']) {
        return Err("Invalid skill name".into());
    }
    let skill = discover_skills(f, root, Some(name))
        .pop()
        .ok_or_else(|| format!("Skill not found: {name}"))?;
    if skill["oversized"] == true {
        return Err("Skill is too large to load (maximum 131072 bytes)".into());
    }
    let description = skill["description"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| format!("\nDescription: {s}"))
        .unwrap_or_default();
    Ok(format!(
        "Skill: {name}{description}\n\n{}",
        field(&skill, "body")
    ))
}

pub(super) fn register(manager: &RpcHandlerManager, root: &Path) {
    for method in ["listSlashCommands", "listSkills"] {
        let root = root.to_owned();
        manager.register(method, move |v| {
            let root = root.clone();
            async move {
                let reply = tokio::task::spawn_blocking(move || {
                    if method == "listSkills" {
                        let flavor = v["flavor"]
                            .as_str()
                            .unwrap_or("claude")
                            .trim()
                            .to_lowercase();
                        let items = skills(&flavor, &root);
                        json!({"success":true,"skills":items})
                    } else {
                        let items = commands(field(&v, "agent"), &root);
                        json!({"success":true,"commands":items})
                    }
                })
                .await
                .map_err(|e| e.to_string())?;
                Ok(Some(reply))
            }
        });
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn write(path: &Path, body: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap()
    }
    #[test]
    fn commands_and_skills() {
        use std::os::unix::fs::symlink;
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
        symlink(
            root.join(".agents/skills/one"),
            child.join(".agents/skills/link"),
        )
        .unwrap();
        let result = skills("claude", &child);
        assert_eq!(result.iter().filter(|v| v["name"] == "example").count(), 1);
        fs::remove_file(child.join(".agents/skills/link")).unwrap();
        assert!(skills("claude", &child).contains(&json!({"name":"example","description":"near"})));
        fs::remove_dir(root.join(".git")).unwrap();
        assert_eq!(projects(&child), vec![child.as_path()]);
    }
}

#[cfg(test)]
mod frontmatter_tests {
    use super::*;
    #[test]
    fn installed_skill_and_command_excerpts() {
        let (skill, body) = frontmatter(include_str!("fixtures/context7-SKILL.md"));
        assert_eq!(skill["name"], "context7-mcp");
        assert!(field(&skill, "description").ends_with("Prisma, Supabase, etc."));
        assert!(body.starts_with("When the user asks"));
        let (command, body) = frontmatter(include_str!("fixtures/spec-init.md"));
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
            "---\nname: fine\n---suffix\nbody",
        ] {
            assert_eq!(frontmatter(raw), (Value::Null, raw.into()));
        }
        assert_eq!(frontmatter("---\nname: skill\n---").0["name"], "skill");
    }
}
