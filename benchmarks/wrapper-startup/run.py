"""Linux: python3 benchmarks/wrapper-startup/run.py path/to/hapi [--rounds 20]."""
import argparse
import contextlib
import json
import math
import os
from pathlib import Path
import signal
import socket
import statistics
import subprocess
import tempfile
import time
import urllib.request
import uuid


FLAVORS = ["claude", "codex", "opencode", "pi"]
AGENT = """#!/bin/sh
case "$1" in
    --version|version|-v)
        case "${0##*/}" in codex) echo 'codex-cli 0.154.0';; *) echo '1.2.0';; esac
        exit 0;;
esac
case "${0##*/}: $* " in
    claude:*" --fork-session "*|codex:*" app-server "*|opencode:*" acp "*|pi:*" --mode rpc "*) ;;
    *) exit 0;;
esac
echo ready > "$WRAPPER_MARKER"
exec sleep 60
"""


@contextlib.contextmanager
def launch(args, env, work):
    with tempfile.TemporaryFile() as log:
        proc = subprocess.Popen(args, env=env, cwd=work, start_new_session=True,
                                stdin=subprocess.DEVNULL, stdout=log, stderr=log)
        try:
            yield proc
        except Exception:
            log.seek(0)
            print(log.read().decode(errors="replace")[-2000:])
            raise
        finally:
            with contextlib.suppress(ProcessLookupError):
                os.killpg(proc.pid, signal.SIGKILL)
            proc.wait()


def wait_until(proc, ready):
    deadline = time.monotonic() + 25
    while proc.poll() is None and time.monotonic() < deadline:
        if ready():
            return
        time.sleep(0.002)
    raise RuntimeError(f"{proc.args} failed to reach checkpoint (exit {proc.poll()})")


def healthy(url):
    try:
        with urllib.request.urlopen(url + "/health", timeout=0.2) as resp:
            return resp.status == 200
    except OSError:
        return False


def run(binary, rounds, root):
    home = root / "hapi"
    home.mkdir()
    for flavor in FLAVORS:
        shim = root / flavor
        shim.write_text(AGENT)
        shim.chmod(0o755)
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        port = s.getsockname()[1]
    url = f"http://127.0.0.1:{port}"
    marker = root / "agent-ready"
    env = dict(PATH=f'{root}:{os.environ["PATH"]}', HOME=str(root), HAPI_HOME=str(home),
               HAPI_API_URL=url, CLI_API_TOKEN=str(uuid.uuid4()))
    results = {f: [] for f in FLAVORS}
    with launch([binary, "hub", "--port", str(port)], env, root) as hub:
        wait_until(hub, lambda: healthy(url))
        with launch([binary, "runner", "start-sync"], env, root) as runner:
            wait_until(runner, (home / "runner.state.json").exists)
            for i in range(rounds + 1):
                for j in range(len(FLAVORS)):
                    flavor = FLAVORS[(i + j) % len(FLAVORS)]
                    args = [binary, flavor, "--started-by", "runner"]
                    if flavor != "codex":
                        args += ["--hapi-starting-mode", "remote"]
                    if flavor == "claude":
                        args += ["--fork-session"]
                    marker.unlink(missing_ok=True)
                    start = time.monotonic_ns()
                    with launch(args, {**env, "WRAPPER_MARKER": str(marker)}, root) as proc:
                        wait_until(proc, marker.exists)
                        ms = (time.monotonic_ns() - start) / 1e6
                        samples = []
                        for delay in (1, 0.25, 0.25):
                            time.sleep(delay)
                            rollup = Path(f"/proc/{proc.pid}/smaps_rollup").read_text()
                            samples.append(int(next(line.split()[1] for line in rollup.splitlines()
                                                    if line.startswith("Pss:"))))
                        if i:
                            results[flavor].append((ms, statistics.median(samples) / 1024))
    for flavor, rows in results.items():
        latency, memory = zip(*rows)
        print(json.dumps(dict(flavor=flavor, n=rounds, p50_ms=statistics.median(latency),
                              p95_ms=sorted(latency)[math.ceil(rounds * 0.95) - 1],
                              pss_mib=statistics.median(memory), samples=rows)))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--rounds", type=int, default=20)
    args = parser.parse_args()
    if args.rounds < 1:
        parser.error("rounds must be positive")
    with tempfile.TemporaryDirectory(prefix="wrapper-startup-") as tmp:
        run(str(args.binary.resolve(strict=True)), args.rounds, Path(tmp))
