"""Validate and summarize all four run.sh outputs; keep the underlying counters."""
import argparse
import json
import math
from pathlib import Path
from statistics import fmean

PROFILES = {
    "http-ts-ten": "HTTP light, TS scaling baseline",
    "http-ts-single": "HTTP light, sole TS session",
    "http-rust-ten": "HTTP light, paired",
    "http-rust-single": "HTTP light, sole Rust session",
    "https-light-ten": "HTTPS light, paired",
    "https-loaded-ten": "HTTPS loaded, paired",
}


def load(directory):
    files = [json.loads((directory / (name + ".json")).read_text())
             for name in ("http-ts", "http-rust", "https-light", "https-loaded")]
    result = {key: files[0][key] for key in ("revision", "artifacts")}
    runs = []
    for data in files:
        if any(data[k] != result[k] for k in ("revision", "artifacts")):
            raise ValueError("Artifact identity changed between cohorts")
        runs.extend(data["runs"])
    if [r["label"] for r in runs] != list(PROFILES):
        raise ValueError("Incomplete or unexpected cohort matrix")
    for run in runs:
        single = run["label"].endswith("single")
        loaded = run["label"] == "https-loaded-ten"
        expected_runtimes = {"ts", "rust"}
        if run["label"].startswith("http-ts"):
            expected_runtimes = {"ts"}
        elif run["label"] == "http-rust-single":
            expected_runtimes = {"rust"}
        assert set(run["cohorts"]) == expected_runtimes
        assert run["turns"] == (30 if loaded else 1)
        assert run["openTerminals"] == (20 if loaded else 0)
        for group in run["cohorts"].values():
            assert len(group["samples"]) == len(group["ids"]) == (1 if single else 10)
            assert len(group["latencyMs"]) == (0 if single else 20)
            if single:
                assert run["singleton_executable_pids_before"] == run["singleton_executable_pids_after"] == group["pids"]
            if loaded:
                assert all(1_000_000 <= size < 1_200_000 for size in group["transcriptBytes"])
            for i, s in enumerate(group["samples"]):
                assert s["pid"] == group["pids"][i] and s["elapsed_ns"] >= 60_000_000_000
                assert run["window_start_ms"] <= s["active_at_ms"] <= run["window_end_ms"] and s["active_after"]
                mem = s["memory_kib"]
                assert mem["USS"] == mem["Private_Clean"] + mem["Private_Dirty"]
                assert mem["USS"] <= mem["Pss"] <= mem["Rss"]
    receipt = json.loads((directory / "compiled.build.json").read_text())
    if receipt["revision"] != result["revision"] or receipt["sha256"] != result["artifacts"]["ts"]["sha256"]:
        raise ValueError("Compiled build receipt does not match measured executable")
    return dict(**result, compiled_build=receipt, runs=runs)


def table(headers, rows):
    print("| " + " | ".join(headers) + " |")
    print("|" + "|".join(["---"] * len(headers)) + "|")
    for row in rows:
        print("| " + " | ".join(map(str, row)) + " |")
    print()


def percentile(values, p):
    return sorted(values)[math.ceil(len(values) * p) - 1]


def summarize(data):
    memory, split, verdicts, cpu, latency = [], [], [], [], []
    for run in data["runs"]:
        case = PROFILES[run["label"]]
        for runtime, group in run["cohorts"].items():
            samples = group["samples"]
            label = [case, runtime.upper(), len(samples)]
            def mean_mem(key):
                return f"{fmean(s['memory_kib'][key] for s in samples):,.1f}"
            memory.append(label + [mean_mem(k) for k in ("Rss", "Pss", "USS")] +
                          [f"{fmean(s[k] for s in samples):,.1f}" for k in ("hapi_pss_kib","group_pss_kib")])
            split.append(label + [mean_mem(k) for k in ("Pss_Anon","Pss_File","Private_Clean","Private_Dirty","Swap","SwapPss")])
            full_cpu = [s["cpu_ns"] * 100 / s["elapsed_ns"] for s in samples]
            switches = [s["all_thread_switches"] * 1e9 / s["elapsed_ns"] for s in samples]
            ticks = fmean(s["cpu_ticks"] * 100e9 / (run["clk_tck"] * s["elapsed_ns"]) for s in samples)
            cpu.append(label + [f"{fmean(full_cpu):.6f}", "< 0.017" if ticks == 0 else f"{ticks:.6f}",
                                f"{fmean(s['observed_schedstat_cpu_ns'] * 100 / s['elapsed_ns'] for s in samples):.6f}",
                                f"{fmean(s['leader_schedstat'] * 1e9 / s['elapsed_ns'] for s in samples):.6f}",
                                f"{fmean(switches):.6f}"])
            if runtime == "rust":
                maxima = [max(s["memory_kib"][k] for s in samples) for k in ("Pss","Rss","USS")]
                comparisons = [f"{v:,} · {'pass' if v*1024 <= 15_000_000 else 'above'}" for v in maxima]
                verdicts.append([case] + comparisons + [f"{max(full_cpu):.6f}",f"{max(switches):.6f}",
                    "Pass" if max(full_cpu)<=0.1 and max(switches)<=1 else "Inclusive budget exceeded"])
        if all(k in run["cohorts"] for k in ("ts","rust")):
            tt,rr=(run["cohorts"][r]["latencyMs"] for r in ("ts","rust"))
            latency.append([case,percentile(tt,.5),percentile(rr,.5),percentile(tt,.95),percentile(rr,.95),
                            "Pass" if percentile(rr,.5)<=percentile(tt,.5) else "Fail"])
        elif run["cohorts"].get("ts", {}).get("latencyMs"):
            tt = run["cohorts"]["ts"]["latencyMs"]
            latency.append([case, percentile(tt,.5), "—", percentile(tt,.95), "—", "TS-only baseline"])
    print("### Wrapper and session memory (mean KiB)\n")
    table(["Condition","Wrapper","N","RSS","PSS","USS","HAPI-owned PSS/session","PSS/session with fixture"],memory)
    print("### Wrapper memory components (mean KiB)\n")
    table(["Condition","Wrapper","N","PSS anon","PSS file","Private clean","Private dirty","Swap","SwapPss"],split)
    print("### Idle CPU and scheduling (cohort means)\n")
    table(["Condition","Wrapper","N","Whole CPU %","Tick CPU %","Observed schedstat CPU %","Leader dispatches/s","Whole switches/s"],cpu)
    print("### S1 and S2 checks for the sampled Rust wrappers\n")
    table(["Rust condition","S1 max PSS (KiB)","Max RSS vs 15 MB","Max USS vs 15 MB","Max CPU %","Max switches/s","S2 inclusive proxy"],verdicts)
    print("### S8 fresh spawn to webhook (ms)\n")
    table(["Fresh-start cohort","TS p50","Rust p50","TS p95","Rust p95","S8 latency only"],latency)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directory", type=Path)
    parser.add_argument("--json", type=Path, help="write consolidated raw samples")
    args = parser.parse_args()
    data = load(args.directory)
    if args.json:
        args.json.write_text(json.dumps(data, indent=2) + "\n")
    summarize(data)
