#!/usr/bin/env python3
import json
import os
import re
import sys
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
turn = len(transcript.read_text().splitlines()) if resume else 0
first = True
for line in sys.stdin:
    frame = json.loads(line)
    if frame["type"] == "control_request":
        emit(type="control_response", response={"subtype": "success", "request_id": frame["request_id"], "response": {}})
    elif frame["type"] == "user":
        turn += 1
        text = frame["message"]["content"]
        with transcript.open("a") as history:
            history.write(json.dumps(frame) + "\n")
        if first:
            emit(type="system", subtype="init", session_id=native, model="fixture")
            first = False
        emit(**frame, isReplay=True)
        emit(type="assistant", message={"role": "assistant", "content": [{"type": "text", "text": f"fixture turn {turn}: {text}"}]})
        emit(type="result", subtype="success", duration_ms=1, num_turns=1, is_error=False)
