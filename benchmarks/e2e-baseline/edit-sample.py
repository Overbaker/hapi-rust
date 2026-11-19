"""Only sizes leave the input files; supplied local transcripts are opt-in."""
import json
import os
from pathlib import Path

def pairs(x):
    if isinstance(x, list):
        for element in x:
            yield from pairs(element)
    elif isinstance(x, dict):
        for a,b in [('oldString','newString'),('old_string','new_string'),('oldText','newText')]:
            if isinstance(x.get(a),str) and isinstance(x.get(b),str):
                yield x[a],x[b]
                return
        for value in x.values():
            yield from pairs(value)

def sizes(source, chunks):
    seen=set()
    for record in chunks:
        for a,b in pairs(record):
            if (a,b) in seen: continue
            seen.add((a,b))
            print(json.dumps(dict(kind='editSize',source=source,oldBytes=len(a.encode()),newBytes=len(b.encode()),
                oldLines=len(a.splitlines()),newLines=len(b.splitlines()))))

for file in sorted(Path('cli/src/agent/backends/acp/__fixtures__').glob('*edit-file.json')):
    sizes(file.name,[json.loads(file.read_text())])
if os.environ.get('BASELINE_CLAUDE_JSONL'):
    with open(os.environ['BASELINE_CLAUDE_JSONL']) as file:
        sizes('authorized Claude JSONL',(json.loads(line) for line in file if line.strip()))
