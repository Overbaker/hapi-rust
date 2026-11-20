#!/usr/bin/env python3
import json
import os
import re
import sys
import subprocess
import shutil
import shlex
import time
import uuid
from pathlib import Path


def emit(**message):
    print(json.dumps(message), flush=True)


if "--print" in sys.argv:
    emit(type="system", subtype="init", tools=["Read"], slash_commands=["compact"])
    sys.exit(0)

project = re.sub(r"[^a-zA-Z0-9]", "-", os.getcwd())
directory = Path(os.environ["CLAUDE_CONFIG_DIR"], "projects", project)
directory.mkdir(parents=True, exist_ok=True)
resume = "--resume" in sys.argv
native = sys.argv[sys.argv.index("--resume") + 1] if resume else str(uuid.uuid4())
transcript = directory / (native + ".jsonl")
if resume and not transcript.is_file():
    sys.exit("missing native transcript for resume")
if resume and os.getenv("TIER_B_ROTATE"):
    native = str(uuid.uuid4())
    rotated = directory / (native + ".jsonl")
    shutil.copyfile(transcript, rotated)
    transcript = rotated
    settings = json.loads(Path(sys.argv[sys.argv.index("--settings") + 1]).read_text())
    command = settings["hooks"]["SessionStart"][0]["hooks"][0]["command"]
    subprocess.run(shlex.split(command), input=json.dumps({"hook_event_name":"SessionStart",
                   "session_id":native, "source":"resume", "cwd":os.getcwd()}), text=True, check=True)
    deadline = time.monotonic() + 30
    marker = Path(os.environ["TIER_B_ROTATE"], native + ".continue")
    while not marker.exists():
        if time.monotonic() >= deadline:
            sys.exit("rotated SessionStart was not acknowledged")
        time.sleep(0.05)
history_rows = (json.loads(line) for line in transcript.read_text().splitlines()) if resume else ()
turn = sum(row.get("type") == "user" and isinstance(row["message"]["content"], str) for row in history_rows)
first = True
for line in sys.stdin:
    frame = json.loads(line)
    if frame["type"] == "control_request":
        emit(type="control_response", response={"subtype": "success", "request_id": frame["request_id"], "response": {}})
    elif frame["type"] == "user":
        if first and os.getenv("TIER_B_DESCENDANTS"):
            children = [subprocess.Popen(["sleep", "3600"], start_new_session=outside)
                        for outside in [False, True]]
            Path(os.environ["TIER_B_DESCENDANTS"]).write_text(json.dumps([p.pid for p in children]))
        turn += 1
        text = frame["message"]["content"]
        with transcript.open("a") as history:
            history.write(json.dumps(frame) + "\n")
        if first:
            emit(type="system", subtype="init", session_id=native, model="fixture")
            first = False
        emit(**frame, isReplay=True)
        if os.getenv("TIER_B_LOADED"):
            tool = {"type":"assistant","message":{"role":"assistant","content":[
                {"type":"tool_use","id":f"read-{turn}","name":"Read","input":{"file_path":"fixture.txt"}}]}}
            result = {"type":"user","message":{"role":"user","content":[
                {"type":"tool_result","tool_use_id":f"read-{turn}","content":"x"*34500}]}}
            for row in (tool, result):
                row["uuid"] = str(uuid.uuid4())
                emit(**row)
                with transcript.open("a") as history:
                    history.write(json.dumps(row) + "\n")
        emit(type="assistant", message={"role": "assistant", "content": [{"type": "text", "text": f"fixture turn {turn}: {text}"}]})
        emit(type="result", subtype="success", duration_ms=1, num_turns=1, is_error=False)
