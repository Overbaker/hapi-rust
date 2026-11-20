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
fn skill_home() -> PathBuf {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(home)
}
fn command_config(key: &str, fallback: PathBuf) -> PathBuf {
    env::var_os(key).map(PathBuf::from).unwrap_or(fallback)
}
fn config(f: &str) -> PathBuf {
    let default = skill_home().join(format!(".{f}"));
    match f {
        "claude" => configured("CLAUDE_CONFIG_DIR", default),
        "codex" => configured("CODEX_HOME", default),
        "grok" => configured("GROK_HOME", default),
        "dsh" => configured("DSH_HOME", default),
        "agy" => skill_home().join(".gemini/config"),
        _ => default,
    }
}
fn projects(root: &Path) -> Vec<PathBuf> {
    let Ok(absolute) = std::path::absolute(root) else {
        return vec![];
    };
    let root = normalize(&absolute);
    let mut dirs = vec![];
    for p in root.ancestors() {
        dirs.push(p.to_owned());
        if p.join(".git").exists() {
            return dirs;
        }
    }
    vec![root]
}
fn install_time(stamp: &str) -> Option<i64> {
    let (date, clock) = stamp.split_once(['T', 't']).unwrap_or((stamp, "00:00:00"));
    let mut date = date.split('-');
    let year: i64 = date.next()?.parse().ok()?;
    let month: usize = date.next()?.parse().ok()?;
    let day: i64 = date.next()?.parse().ok()?;
    if date.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let clock = clock.trim_end_matches(['Z', 'z']);
    let offset = clock.find(['+', '-']);
    let (clock, zone) = clock.split_at(offset.unwrap_or(clock.len()));
    let zone_minutes = if zone.is_empty() {
        0
    } else {
        let digits = zone[1..].replace(':', "");
        if digits.len() != 4 {
            return None;
        }
        let hours: i64 = digits.get(..2)?.parse().ok()?;
        let minutes: i64 = digits.get(2..)?.parse().ok()?;
        if hours > 23 || minutes > 59 {
            return None;
        }
        (hours * 60 + minutes) * if zone.starts_with('-') { -1 } else { 1 }
    };
    let mut clock = clock.split(':');
    let hour: i64 = clock.next()?.parse().ok()?;
    let minute: i64 = clock.next()?.parse().ok()?;
    let seconds = clock.next().unwrap_or("0");
    let (second, fraction) = seconds.split_once('.').unwrap_or((seconds, ""));
    let second: i64 = second.parse().ok()?;
    if !(0..=9999).contains(&year)
        || !(0..=24).contains(&hour)
        || (hour == 24 && (minute != 0 || second != 0 || fraction.bytes().any(|b| b != b'0')))
        || !(0..=59).contains(&minute)
        || !(0..=59).contains(&second)
        || clock.next().is_some()
        || !fraction.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let millis = fraction
        .bytes()
        .chain(std::iter::repeat(b'0'))
        .take(3)
        .fold(0_i64, |n, b| n * 10 + i64::from(b - b'0'));
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let prev = year - 1;
    let days = 365 * prev + prev.div_euclid(4) - prev.div_euclid(100)
        + prev.div_euclid(400)
        + [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334][month - 1]
        + i64::from(leap && month > 2)
        + day
        - 719163;
    Some((((days * 24 + hour) * 60 + minute - zone_minutes) * 60 + second) * 1000 + millis)
}

fn plugins(config_dir: &Path, skills: bool) -> Vec<(String, PathBuf)> {
    let data = fs::read(config_dir.join("plugins/installed_plugins.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .unwrap_or_default();
    data["plugins"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(name, installs)| {
            let installs = installs.as_array()?;
            let time = |install: &Value| {
                install["lastUpdated"]
                    .as_str()
                    .and_then(install_time)
                    .or_else(|| {
                        skills
                            .then(|| install["installedAt"].as_str().and_then(install_time))
                            .flatten()
                    })
            };
            let mut latest = installs.first()?;
            for candidate in &installs[1..] {
                let (a, b) = (time(candidate), time(latest));
                let newer = if skills {
                    a.unwrap_or(0) > b.unwrap_or(0)
                } else {
                    a.zip(b).is_some_and(|(a, b)| a > b)
                };
                if newer {
                    latest = candidate;
                }
            }
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
fn js_trim(s: &str) -> &str {
    s.trim_matches(|c: char| c == '\u{feff}' || (c != '\u{85}' && c.is_whitespace()))
}

fn yaml_fields(head: &str, mut clone_budget: usize) -> Option<Value> {
    use yaml_rust2::parser::{Event, MarkedEventReceiver, Parser};
    use yaml_rust2::scanner::TScalarStyle;
    let mut parser = Parser::new_from_str(head);
    let mut loader = yaml_rust2::YamlLoader::default();
    let mut parents = Vec::new();
    let mut anchors = BTreeMap::new();
    loop {
        let (mut event, mark) = parser.next_token().ok()?;
        let completed = match &mut event {
            Event::Scalar(value, style, anchor, tag) => {
                if *style == TScalarStyle::Plain
                    && tag.is_none()
                    && matches!(value.as_str(), "NULL" | "Null")
                {
                    value.make_ascii_lowercase();
                }
                Some((*anchor, value.len() + 64))
            }
            Event::MappingStart(anchor, _) | Event::SequenceStart(anchor, _) => {
                if parents.len() >= 64 {
                    return None;
                }
                parents.push((*anchor, 64usize));
                None
            }
            Event::MappingEnd | Event::SequenceEnd => Some(parents.pop()?),
            Event::Alias(anchor) => {
                let weight = *anchors.get(anchor)?;
                clone_budget = clone_budget.checked_sub(weight)?;
                Some((0, weight))
            }
            Event::DocumentEnd => {
                loader.on_event(event, mark);
                loader.documents().first()?;
                continue;
            }
            Event::DocumentStart if !loader.documents().is_empty() => return None,
            Event::StreamEnd => break,
            _ => None,
        };
        if let Some((anchor, weight)) = completed {
            if anchor != 0 {
                clone_budget = clone_budget.checked_sub(weight)?;
                anchors.insert(anchor, weight);
            }
            if let Some((_, parent)) = parents.last_mut() {
                *parent = parent.saturating_add(weight);
            }
        }
        loader.on_event(event, mark);
    }
    let mut fields = json!({});
    if let Some(doc) = loader.documents().first() {
        for k in ["name", "description"] {
            if let Some(value) = doc[k].as_str() {
                fields[k] = json!(value)
            }
        }
    }
    Some(fields)
}

fn frontmatter(s: &str) -> (Value, String) {
    let fallback = || (Value::Null, js_trim(s).to_owned());
    let Some(rest) = s
        .strip_prefix("---\n")
        .or_else(|| s.strip_prefix("---\r\n"))
    else {
        return fallback();
    };
    if let Some(offset) = rest.find("\n---") {
        let head = rest[..offset].strip_suffix('\r').unwrap_or(&rest[..offset]);
        let Some(fields) = yaml_fields(head, 1024 * 1024) else {
            return fallback();
        };
        return (fields, js_trim(&rest[offset + 4..]).to_owned());
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
    let mut builtins = BTreeMap::new();
    builtins.insert("claude".into(), vec![
        json!({"name": "clear", "description": "Clear conversation history and free up context", "source": "builtin"}),
        json!({"name": "compact", "description": "Clear conversation history but keep a summary in context", "source": "builtin"}),
        json!({"name": "context", "description": "Visualize current context usage as a colored grid", "source": "builtin"}),
        json!({"name": "cost", "description": "Show the total cost and duration of the current session", "source": "builtin"}),
        json!({"name": "doctor", "description": "Diagnose and verify your Claude Code installation and settings", "source": "builtin"}),
        json!({"name": "plan", "description": "View or open the current session plan", "source": "builtin"}),
        json!({"name": "stats", "description": "Show your Claude Code usage statistics and activity", "source": "builtin"}),
        json!({"name": "status", "description": "Show Claude Code status including version, model, account, and API connectivity", "source": "builtin"}),
    ]);
    builtins.insert("codex".into(), vec![
        json!({"name": "agent", "description": "Toggle proactive Codex multi-agent delegation", "source": "builtin"}),
        json!({"name": "clear", "description": "Clear current Codex thread context", "source": "builtin"}),
        json!({"name": "compact", "description": "Compact current Codex thread context", "source": "builtin"}),
        json!({"name": "goal", "description": "Set, view, pause, resume, or clear a persistent Codex goal", "source": "builtin"}),
        json!({"name": "help", "description": "Show supported HAPI Codex slash commands", "source": "builtin"}),
        json!({"name": "plan", "description": "Enable plan mode; use /plan off to return to default", "source": "builtin"}),
        json!({"name": "default", "description": "Return Codex collaboration mode to default", "source": "builtin"}),
        json!({"name": "execute", "description": "Return Codex collaboration mode to default", "source": "builtin"}),
        json!({"name": "status", "description": "Show current Codex session config", "source": "builtin"}),
        json!({"name": "model", "description": "Show or set Codex model, e.g. /model gpt-5.5", "source": "builtin"}),
        json!({"name": "reasoning", "description": "Show or set reasoning effort", "source": "builtin"}),
        json!({"name": "effort", "description": "Alias for /reasoning", "source": "builtin"}),
        json!({"name": "personality", "description": "Show or set response style: friendly, pragmatic, or none", "source": "builtin"}),
        json!({"name": "permissions", "description": "Show or set permission mode", "source": "builtin"}),
        json!({"name": "permission", "description": "Alias for /permissions", "source": "builtin"}),
    ]);
    builtins.insert("gemini".into(), vec![
        json!({"name": "about", "description": "Show version info", "source": "builtin"}),
        json!({"name": "clear", "description": "Clear the screen and conversation history", "source": "builtin"}),
        json!({"name": "compress", "description": "Compress the context by replacing it with a summary", "source": "builtin"}),
        json!({"name": "stats", "description": "Check session stats", "source": "builtin"}),
    ]);
    builtins.insert("grok".into(), vec![
        json!({"name": "compact", "description": "Compress conversation history to save context", "source": "builtin"}),
        json!({"name": "context", "description": "Show context window usage and session stats", "source": "builtin"}),
        json!({"name": "session-info", "description": "Show Grok session model, turns, and context usage", "source": "builtin"}),
        json!({"name": "goal", "description": "Set, manage, or inspect an autonomous goal", "source": "builtin"}),
        json!({"name": "always-approve", "description": "Toggle automatic tool approval", "source": "builtin"}),
        json!({"name": "auto", "description": "Let Grok classify safe tool calls for automatic approval", "source": "builtin"}),
    ]);
    builtins.insert("opencode".into(), vec![
        json!({"name": "help", "description": "Show supported HAPI OpenCode slash commands", "source": "builtin"}),
        json!({"name": "status", "description": "Show current OpenCode session config", "source": "builtin"}),
        json!({"name": "plan", "description": "Enable plan mode; use /plan off to return to default", "source": "builtin"}),
        json!({"name": "default", "description": "Return OpenCode permission mode to default", "source": "builtin"}),
        json!({"name": "init", "description": "Generate or refresh AGENTS.md for this project", "source": "builtin"}),
    ]);
    builtins.insert("cursor".into(), vec![
        json!({"name": "compress", "description": "Compress conversation context to free window space (pass-through to Cursor agent)", "source": "builtin"}),
    ]);
    builtins.insert("copilot".into(), vec![
        json!({"name": "help", "description": "Show supported Copilot slash commands", "source": "builtin"}),
        json!({"name": "status", "description": "Show current Copilot session config", "source": "builtin"}),
        json!({"name": "plan", "description": "Start plan mode for structured implementation planning", "source": "builtin"}),
        json!({"name": "autopilot", "description": "Start autopilot mode for autonomous multi-step work", "source": "builtin"}),
        json!({"name": "fleet", "description": "Run parallel subagents for a task (combine with Interactive/Plan/Autopilot)", "source": "builtin"}),
        json!({"name": "tasks", "description": "List or manage fleet tasks", "source": "builtin"}),
        json!({"name": "subagents", "description": "Manage Copilot subagents", "source": "builtin"}),
        json!({"name": "agents", "description": "Alias for /subagents", "source": "builtin"}),
        json!({"name": "delegate", "description": "Delegate work to a subagent", "source": "builtin"}),
        json!({"name": "agent", "description": "Select or configure a custom agent", "source": "builtin"}),
        json!({"name": "rubber-duck", "description": "Consult the rubber-duck agent for a second opinion on plans, code, and tests", "source": "builtin"}),
        json!({"name": "security-review", "description": "Run a focused security review of active local code changes", "source": "builtin"}),
        json!({"name": "research", "description": "Run a deep research investigation across the codebase and web", "source": "builtin"}),
        json!({"name": "review", "description": "Run the code-review agent on current changes", "source": "builtin"}),
        json!({"name": "skills", "description": "List, inspect, add, or remove Copilot skills", "source": "builtin"}),
        json!({"name": "context", "description": "Show current context usage", "source": "builtin"}),
        json!({"name": "model", "description": "Show or switch the active model", "source": "builtin"}),
        json!({"name": "permissions", "description": "Show or set permission mode", "source": "builtin"}),
        json!({"name": "permission", "description": "Alias for /permissions", "source": "builtin"}),
        json!({"name": "usage", "description": "Show session usage metrics", "source": "builtin"}),
    ]);
    builtins.insert("kimi".into(), vec![]);
    builtins.insert("agy".into(), vec![
        json!({"name": "goal", "description": "Run until the specified goal is completely finished.", "source": "builtin"}),
        json!({"name": "compact", "description": "Compact the conversation now (summarize history to free up the context window).", "source": "builtin"}),
        json!({"name": "schedule", "description": "Run an instruction on a recurring schedule or as a one-time timer.", "source": "builtin"}),
        json!({"name": "browser", "description": "Invoke a browser agent for web tasks.", "source": "builtin"}),
        json!({"name": "plan", "description": "Plan carefully before executing a task.", "source": "builtin"}),
        json!({"name": "grill-me", "description": "Interview me to align on a plan.", "source": "builtin"}),
        json!({"name": "teamwork", "description": "Invoke a team of agents to autonomously tackle large projects.", "source": "builtin"}),
        json!({"name": "learn", "description": "Reflect on recent successes or corrections to capture reusable skills or rules.", "source": "builtin"}),
        json!({"name": "boost", "description": "Invoke the Boost multi-agent orchestrator for complex tasks.", "source": "builtin"}),
        json!({"name": "swarm", "description": "(Preview & Quota Heavy) Run a swarm: several agents working in parallel on one task, each in its own conversation, collaborating through a shared message board.", "source": "builtin"}),
    ]);
    builtins.insert("pi".into(), vec![
        json!({"name": "help", "description": "Show supported HAPI Pi slash commands", "source": "builtin"}),
        json!({"name": "compact", "description": "Compress conversation history to save context (optional custom instructions)", "source": "builtin"}),
        json!({"name": "session", "description": "Show Pi session stats (tokens, cost, context usage)", "source": "builtin"}),
        json!({"name": "model", "description": "Show or switch the Pi model, e.g. /model gpt-5.2", "source": "builtin"}),
    ]);
    builtins
});
fn commands(f: &str, root: &Path) -> Vec<Value> {
    let mut out = BUILTINS.get(f).unwrap_or(&BUILTINS["claude"]).clone();
    let (global, local) = match f {
        "claude" => (
            command_config("CLAUDE_CONFIG_DIR", home().join(".claude")).join("commands"),
            ".claude/commands",
        ),
        "codex" => (
            command_config("CODEX_HOME", home().join(".codex")).join("prompts"),
            ".codex/prompts",
        ),
        "opencode" => (
            command_config("XDG_CONFIG_HOME", home().join(".config")).join("opencode/command"),
            ".opencode/command",
        ),
        "grok" => (home().join(".agents/commands"), ".grok/commands"),
        _ => return out,
    };
    out.extend(sorted_commands(&global, "user", None));
    if f == "claude" {
        let mut installed = vec![];
        for (name, p) in plugins(
            &command_config("CLAUDE_CONFIG_DIR", home().join(".claude")),
            false,
        ) {
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
    for entry in fs::read_dir(root).into_iter().flatten().flatten() {
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
const MAX_SKILL_BYTES: usize = 128 * 1024;

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
    roots.push(skill_home().join(".agents/skills"));
    if matches!(f, "claude" | "codex" | "grok" | "copilot" | "dsh" | "agy") {
        roots.push(config(f).join("skills"))
    }
    if f == "agy" {
        roots.push(skill_home().join(".gemini/antigravity-cli/skills"))
    }
    let mut dirs = vec![];
    for root in roots {
        dirs.extend(skill_dirs(
            &root,
            f == "codex" && (root.ends_with(".codex/skills") || root == config(f).join("skills")),
        ))
    }
    for (_, plugin) in plugins(&config(f), true) {
        dirs.extend(skill_dirs(&plugin.join("skills"), false))
    }
    if f == "codex" {
        dirs.extend(skill_dirs(Path::new("/etc/codex/skills"), true))
    }
    let mut found = BTreeMap::new();
    for p in dirs {
        let mut open = fs::OpenOptions::new();
        open.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            open.custom_flags(nix::libc::O_NONBLOCK);
        }
        let Ok(file) = open.open(p.join("SKILL.md")) else {
            continue;
        };
        let Ok(info) = file.metadata() else { continue };
        if !info.is_file() {
            continue;
        }
        let mut bytes = vec![];
        if file
            .take(MAX_SKILL_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .is_err()
        {
            continue;
        }
        let (fields, body) = frontmatter(&String::from_utf8_lossy(&bytes));
        let mut name = js_trim(field(&fields, "name"));
        if name.is_empty() {
            let Some(basename) = p.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            name = basename;
        }
        if let Some(target) = lookup {
            if target == name {
                return vec![
                    json!({"name":name,"description":fields["description"],"body":body,"oversized":info.len()>MAX_SKILL_BYTES as u64 || bytes.len()>MAX_SKILL_BYTES}),
                ];
            }
            continue;
        }
        found.entry(name.to_owned()).or_insert_with(|| {
            let mut v = json!({"name":name});
            if let Some(d) = fields["description"].as_str() {
                v["description"] = json!(js_trim(d))
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

pub fn resolve_skill(f: &str, root: &Path, name: &str) -> Result<String, String> {
    let requested_name = name;
    let name = js_trim(name);
    if name.encode_utf16().count() > 128
        || name.is_empty()
        || matches!(name, "." | "..")
        || name.contains(['/', '\\', '\0'])
    {
        return Err("Invalid skill name".into());
    }
    let skill = discover_skills(&js_trim(f).to_lowercase(), root, Some(name))
        .pop()
        .ok_or_else(|| format!("Skill not found: {requested_name}"))?;
    if skill["oversized"] == true {
        return Err("Skill is too large to load (maximum 131072 bytes)".into());
    }
    let description = skill["description"]
        .as_str()
        .map(js_trim)
        .filter(|s| !s.is_empty())
        .map(|s| format!("\nDescription: {s}"))
        .unwrap_or_default();
    Ok(format!(
        "Skill: {name}{description}\n\n{}",
        field(&skill, "body")
    ))
}

pub fn register(manager: &RpcHandlerManager, root: &Path) {
    for method in ["listSlashCommands", "listSkills"] {
        let root = root.to_owned();
        manager.register(method, move |v| {
            let root = root.clone();
            async move {
                let reply = tokio::task::spawn_blocking(move || {
                    if method == "listSkills" {
                        let flavor =
                            js_trim(v["flavor"].as_str().unwrap_or("claude")).to_lowercase();
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
#[path = "tests_discovery.rs"]
mod tests_discovery;
