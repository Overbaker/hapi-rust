#!/usr/bin/env python3
import json
import os
import re
import sys
import subprocess
from pathlib import Path


def send(**value):
    print(json.dumps(value), flush=True)


def record(value):
    with open(os.environ["FAKE_CLAUDE_LOG"], "a") as handle:
        handle.write(json.dumps(value) + "\n")


args = sys.argv[1:]
record({"argv": args, "sid": os.environ.get("HAPI_SESSION_ID"), "pid": os.getpid()})
if "--print" in args:
    send(type="system", subtype="init", tools=["Read"], slash_commands=["review", "status"], skills=["review"])
    sys.exit(0)

native = "native-child" if "--fork-session" in args else "native-session"
project = re.sub(r"[^a-zA-Z0-9]", "-", os.getcwd())
transcript = Path(os.environ["CLAUDE_CONFIG_DIR"], "projects", project, native + ".jsonl")
transcript.parent.mkdir(parents=True, exist_ok=True)
transcript.write_text(json.dumps({"uuid": "fixture", "type": "ai-title", "aiTitle": "Native title"}) + "\n")
if "--fork-session" in args:
    send(type="system", subtype="hook_response", hook_name="SessionStart:fork")

first = True
first_steer = False
deferred_switches = []
for line in sys.stdin:
    frame = json.loads(line)
    record({"input": frame, "pid": os.getpid()})
    if frame["type"] == "control_request":
        request = frame["request"]
        if request["subtype"] == "apply_flag_settings" and os.environ.get("FAKE_SCENARIO") == "effort-exit":
            sys.exit(7)
        if request["subtype"] == "set_permission_mode" and os.environ.get("FAKE_SCENARIO") == "stale-mode":
            deferred_switches.append(frame)
            if len(deferred_switches) == 3:
                for switch in reversed(deferred_switches):
                    stale = switch is deferred_switches[0]
                    reply = {"subtype": "error", "error": "stale switch"} if stale else {"subtype": "success"}
                    send(type="control_response", response={**reply, "request_id": switch["request_id"]})
            continue
        reject_effort = request["subtype"] == "apply_flag_settings" and os.environ.get("FAKE_SCENARIO") == "effort-reject"
        reject = reject_effort or request.get("mode") == "plan" and os.environ.get("FAKE_REJECT_PLAN")
        response = {"subtype": "error", "error": "plan disabled"} if reject else {
            "subtype": "success", "response": {"model": "claude-fixture", "maxTokens": 123456}
        }
        send(type="control_response", response={**response, "request_id": frame["request_id"]})
        continue
    if frame["type"] != "user":
        continue
    text = frame["message"]["content"]
    if text == "crash":
        sys.exit(7)
    if first:
        send(type="system", subtype="init", session_id=native, model="claude-fixture", **({"permissionMode":"default"} if os.environ.get("FAKE_SCENARIO")=="init-mode" else {}))
        first = False
    if text == "unpicked":
        if os.environ.get("FAKE_SCENARIO") == "steer-abort":
            continue
        if first_steer:
            send(type="result", subtype="success")
            sys.exit(0)
    if text == "hold-tree":
        descendant = subprocess.Popen([sys.executable, "-c", "import time;time.sleep(120)"])
        record({"descendant": descendant.pid})
    if text == "steered":
        send(type="result", subtype="success")
    send(**frame, isReplay=True)
    if text in ("hold", "hold-tree"):
        first_steer = True
        continue
    if text.startswith("/compact fail"):
        send(type="system", subtype="status", compact_result="failed", compact_error=text[len("/compact fail"):].strip())
    elif text == "/compact wait":
        send(type="system", subtype="compact_boundary", compact_metadata={"trigger":"manual","post_tokens":20})
        send(type="result",subtype="success")
        send(type="assistant",message={"role":"assistant","content":[{"type":"text","text":"autonomous"}]})
        continue
    elif text.startswith("/compact"):

        send(type="result", subtype="success", duration_api_ms=100, usage={"output_tokens": 1})
        send(type="system", subtype="compact_boundary", compact_metadata={"trigger": "manual", "pre_tokens": 300, "post_tokens": 100})
        with transcript.open("a") as handle:
            handle.write(json.dumps({"type": "user", "isCompactSummary": True, "message": {"content": "new summary"}}) + "\n")
        send(type="user", message={"content": "<local-command-stdout>Compacted </local-command-stdout>"})
    else:
        send(type="assistant", message={"role": "assistant", "content": [{"type": "text", "text": "Answer: " + text}]})
    send(type="result", subtype="success", duration_ms=25, duration_api_ms=20, usage={"output_tokens": 2})
