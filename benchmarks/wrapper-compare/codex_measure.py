"""Run from the repo root on the Linux measurement host with compiled TS and release Rust binaries."""
import argparse
from concurrent.futures import ThreadPoolExecutor
from datetime import datetime, timezone
from hashlib import sha256
import json
import os
from pathlib import Path
import platform
import subprocess
import shutil
import tempfile


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("ts", type=Path)
    parser.add_argument("rust", type=Path)
    parser.add_argument("output", type=Path)
    parser.add_argument("--revision", required=True)
    args = parser.parse_args()
    bins = {"ts": args.ts.resolve(), "rust": args.rust.resolve()}
    harness = "crates/hapi-session-wrapper/src/flavors/codex/runtime_hub.ts"
    result = dict(revision=args.revision, host=platform.uname()._asdict(),
                  utc=datetime.now(timezone.utc).isoformat(), artifacts={}, latency={}, idle={})
    for flavor, exe in bins.items():
        result["artifacts"][flavor] = dict(sha256=sha256(exe.read_bytes()).hexdigest(),
                                           bytes=exe.stat().st_size,
                                           version=subprocess.check_output([exe, "--version"], text=True).strip())
        result["latency"][flavor] = []

    def run(exe, roots=5, warm=False):
        env = dict(os.environ)
        env.pop("CODEX_BENCH_WARM", None)
        env["CODEX_BENCH_ROOTS"] = str(roots)
        if warm:
            env["CODEX_BENCH_WARM"] = "600"
        # The fake runner identity check recognizes the conventional runner argv.
        output = subprocess.check_output(["bun", harness, exe, "measure", "runner", "start-sync"], env=env)
        return json.loads(output)

    for i in range(20):
        for flavor in (list(bins) if i % 2 == 0 else reversed(bins)):
            sample = run(bins[flavor])
            sample.pop("idle")
            result["latency"][flavor].append(sample)
        print(f"Latency pair {i + 1}/20", flush=True)
    args.output.write_text(json.dumps(result, indent=2) + "\n")
    # Separate executable inodes keep one- and five-root PSS independent of
    # each other's executable text while the four cohorts idle on the same host.
    with tempfile.TemporaryDirectory() as tmp, ThreadPoolExecutor(4) as pool:
        workers = {}
        for flavor, exe in bins.items():
            result["idle"][flavor] = {}
            for n in (1, 5):
                target = Path(tmp, flavor + str(n), exe.name)
                target.parent.mkdir()
                shutil.copy2(exe, target)
                assert target.stat().st_ino != exe.stat().st_ino
                assert sha256(target.read_bytes()).hexdigest() == result["artifacts"][flavor]["sha256"]
                workers[flavor, n] = pool.submit(run, target, n, True)
        for (flavor, n), task in workers.items():
            result["idle"][flavor][str(n)] = task.result()["idle"][str(n)]
    args.output.write_text(json.dumps(result, indent=2) + "\n")


if __name__ == "__main__":
    main()
