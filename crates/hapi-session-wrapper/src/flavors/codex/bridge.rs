const TITLE: &str = r##"Use the title tool sparingly. For a new chat, call it once after the user's initial request is clear, and set a concise task title.
Prefer calling functions.hapi__change_title.
If that exact tool name is unavailable, call an equivalent alias such as hapi__change_title, mcp__hapi__change_title, or hapi_change_title.
Do not rename the chat for routine progress, substeps, implementation details, or a slightly better wording.
Rename only when the user's primary objective changes substantially and the existing title would be misleading."##;
const PEERS: &str = r##"When the user cites another HAPI session as [title](/sessions/<id>), Copy-reference prose See session "…" (/sessions/<id>) for context, or a bare /sessions/<id>, extract that <id>. /sessions/<id> is a HAPI hub path, not a local filesystem path - do not Grep, Glob, or Read it as a file. Call "functions.hapi__inspect_peer" with sessionIdPrefix=<id> to read metadata and recent messages; call "functions.hapi__ping_peer" with sessionIdPrefix=<id> and a message to nudge or hand off. Message only peers tied to your current task: a session the user cited, your Parent, or peers you spawned or were handed off to. Never broadcast to a list of peers - not for status updates, and not to announce exclusive work (simulator, device, port, branch, release). For any other peer, call inspect_peer first to see its machine, cwd, and recent work, and message it only when that work actually conflicts with or depends on yours (e.g. same machine and resource, same checkout). Do not wake inactive peers for FYI, status, or done notes. An inactive peer is refused unless you pass wake=true; set it only when the user asked or the handed-off work requires that peer to act. Select deliveryMode: auto (default): steer an active turn when the target supports steering and is ready; otherwise queue. Inactive peers require explicit wake=true and then receive a queued message. Prefer auto for routine collaboration when queueing is an acceptable fallback. If steering cannot be confirmed after acceptance, auto retains the same single accepted message without resending; accepted does not confirm consumption. queue: explicitly defer non-urgent updates that can wait instead of steering. steer: use for important, time-sensitive findings the peer needs to know now, not after finishing its current turn (e.g. a correction that invalidates its assumptions, a blocker, or a result that changes its next action). State why the finding matters now and what the peer should adjust. Steer requires insertion into an active Claude, Pi, Codex, OpenCode, Antigravity, or Cursor ACP turn; unsupported, idle, or inactive targets are rejected before sending. Steering does not abort the current turn or guarantee an immediate response. If send or steer is unconfirmed after submission, inspect the target before retrying; do not resend automatically. Prefer these over JWT+curl. Shell fallbacks: hapi inspect-peer <id> / hapi ping-peer <id> <message> --delivery-mode auto [--wake]. To find a specific peer without a citation, call "functions.hapi__list_peers" (active sessions on the same hub/namespace; works from runner-spawned sessions) - it is a directory, not a recipient list. Shell fallback: hapi ping-peer --list. To spawn a new peer with work attached, call "functions.hapi__spawn_peer" with directory and a required message (the remit). Do not POST /api/machines/.../spawn with a message/prompt/text field - the hub rejects it (HTTP 400). Use spawn_peer instead. Shell fallback: hapi spawn-peer --dir PATH --name TITLE --message-file -."##;
const DISABLED: &str = r##"When the user cites another HAPI session as [title](/sessions/<id>), Copy-reference prose See session "…" (/sessions/<id>) for context, or a bare /sessions/<id>, extract that <id>. /sessions/<id> is a HAPI hub path, not a local filesystem path - do not Grep, Glob, or Read it as a file. Peer session tools (inspect_peer / ping_peer / list_peers) are disabled on this machine; ask the user to paste the relevant context from that session instead."##;
const JOB: &str = r##"Session-attached jobs: default is NOT to use them. Only for a process that must keep running after this agent goes idle and that the operator would reopen the chat to ask about (hours-long batch, nohup script, daemon you own). Never for anything that finishes within this turn: tests, builds, installs, typechecks, git, short scripts. If unsure, do not attach. To attach: Shell `hapi job run <session-id> <job-key> --label <text> --progress unknown -- <cmd>` (use the /sessions/<id> uuid of this chat). Run `hapi job --help` for other modes and rules. Do not pass --wake-on-terminal unless the operator asked to be woken. If you attached a job, clear or fail it yourself when its process is gone."##;
const SUMMARY: &str = r##"Session status summary:
At the end of each turn, append exactly one machine-readable status line
(no backticks) to the final answer only, so this workspace's session tracking
can record progress. Do not emit this line in commentary, progress updates, or before tool calls.
Put it on its own final line after all other content:
AGENT_NOTIFY_SUMMARY {"version":1,"agent":"<agent-id>","project":"<project>","status":"done|blocked|needs_review|needs_decision|failed|stalled","action":"<=12 words","summary":"one-line triage"}
Use the language used by the user in the current conversation for the
human-readable "action" and "summary" values.
Use status "blocked" if unsure. Keep action to 12 words or fewer when status
is "done" and follow-up remains.
After status "done", do not emit further user-visible replies for redundant
background task-complete notifications until the user sends new intent."##;
use hapi_wrapper_core::{
    mcp::{stdio_bridge, tools},
    session::client::SessionClient,
};
use serde_json::{json, Value};
use std::{path::Path, sync::Arc};

pub fn instructions(hub_summary: bool) -> String {
    let disabled = matches!(
        std::env::var("HAPI_DISABLE_PEER_TOOLS")
            .unwrap_or_default()
            .to_lowercase()
            .as_str(),
        "true" | "yes" | "1"
    );
    let summary = std::env::var("HAPI_SESSION_SUMMARY_CONTRACT")
        .ok()
        .filter(|s| !s.is_empty())
        .map_or(hub_summary, |s| {
            !matches!(
                s.trim().to_lowercase().as_str(),
                "0" | "false" | "off" | "no"
            )
        });
    build_prompt(disabled, summary)
}
fn build_prompt(disabled: bool, summary: bool) -> String {
    let mut prompt = format!(
        "{TITLE}\n{}\n\n{JOB}",
        if disabled { DISABLED } else { PEERS }
    );
    if summary {
        prompt.push_str("\n\n");
        prompt.push_str(SUMMARY);
    }
    prompt
}

pub async fn start(
    session: Arc<SessionClient>,
    cwd: &Path,
) -> Result<(tools::Listener, Value), String> {
    let cmd = std::env::current_exe().map_err(|e| e.to_string())?;
    let sid = session.id.clone();
    let listener = tools::start(
        session,
        tools::Options {
            emit_title_summary: false,
            working_directory: Some(cwd.into()),
            skill_lookup: Some((cwd.into(), "codex".into())),
            ..Default::default()
        },
    )
    .await
    .map_err(|e| e.to_string())?;
    let list = stdio_bridge::catalog(None);
    let supported = list["tools"].as_array().unwrap();
    let names = [
        "change_title",
        "display_image",
        "display_video",
        "display_media",
        "list_peers",
        "ping_peer",
        "inspect_peer",
        "spawn_peer",
        "session_job",
        "skill_lookup",
    ]
    .into_iter()
    .filter(|name| supported.iter().any(|t| t["name"] == *name))
    .collect::<Vec<_>>();
    let mut approval = json!({});
    for (name, mode) in [
        ("display_image", "prompt"),
        ("display_video", "prompt"),
        ("display_media", "prompt"),
        ("change_title", "approve"),
        ("list_peers", "approve"),
        ("session_job", "approve"),
        ("skill_lookup", "approve"),
    ] {
        if names.contains(&name) {
            approval[name] = json!({"approval_mode":mode});
        }
    }
    let config = json!({"command":cmd,"args":["mcp","--url",format!("http://127.0.0.1:{}/",listener.port),"--tools",names.join(","),"--hapi-session-id",sid],"tools":approval});
    Ok((listener, config))
}

#[cfg(test)]
#[test]
fn instructions_match_typescript() {
    for disabled in [false, true] {
        for summary in [false, true] {
            let out=std::process::Command::new("bun").current_dir(concat!(env!("CARGO_MANIFEST_DIR"),"/../../cli"))
            .args(["-e","import {getCodexSystemPrompt} from './src/codex/utils/systemPrompt';process.stdout.write(getCodexSystemPrompt())"])
            .env("HAPI_DISABLE_PEER_TOOLS",disabled.to_string()).env("HAPI_SESSION_SUMMARY_CONTRACT",summary.to_string()).output().unwrap();
            assert!(out.status.success());
            assert_eq!(
                build_prompt(disabled, summary),
                String::from_utf8(out.stdout).unwrap()
            );
        }
    }
}
