"""Sample the post-turn idle cohorts created by rustWrapper.integration.test.ts."""
import ctypes
import errno
import json
import os
import platform
import struct
import subprocess
import sys
import tempfile
import time
from pathlib import Path

PROC = Path("/proc")
WARMUP_SECONDS = 600
WINDOW_SECONDS = 60


def stat(path):
    fields = path.read_text().rsplit(") ", 1)[1].split()
    return fields[0], int(fields[1]), int(fields[2]), int(fields[19])


def members(root):
    table = {}
    for path in PROC.iterdir():
        if path.name.isdecimal():
            try:
                state, parent, group, start = stat(path / "stat")
            except (FileNotFoundError, ProcessLookupError):
                continue
            if state != "Z":
                table[int(path.name)] = (parent, group, start)
    owned = {pid for pid, (_, group, _) in table.items() if group == root}
    if root not in owned:
        raise RuntimeError(f"wrapper group {root} disappeared")
    while True:
        children = {pid for pid, (parent, _, _) in table.items() if parent in owned}
        if children <= owned:
            break
        owned |= children
    return {pid: table[pid][2] for pid in owned}


def pss(pid):
    lines = (PROC / str(pid) / "smaps_rollup").read_text().splitlines()
    return next(int(line.split()[1]) for line in lines if line.startswith("Pss:"))


def switch_counters(pids):
    source = Path(__file__).with_name("switch_counter.c")
    with tempfile.TemporaryDirectory() as temp:
        lib = Path(temp) / "switch_counter.so"
        subprocess.run(["cc", "-shared", "-fPIC", "-o", str(lib), str(source)], check=True)
        open_counter = ctypes.CDLL(str(lib), use_errno=True).open_switch_counter
    counters = {}
    for pid in pids:
        deadline = time.monotonic() + 3
        tasks = PROC / str(pid) / "task"
        while True:
            tids = {int(t.name) for t in tasks.iterdir()}
            fds = []
            for tid in tids:
                fd = open_counter(tid)
                if fd < 0:
                    error = ctypes.get_errno()
                    if error not in (errno.ENOENT, errno.ESRCH):
                        raise OSError(error, f"perf_event_open for thread {tid}")
                    break
                fds.append(fd)
            if len(fds) == len(tids) and tids == {int(t.name) for t in tasks.iterdir()}:
                counters[pid] = fds
                break
            for fd in fds:
                os.close(fd)
            if time.monotonic() >= deadline:
                raise RuntimeError(f"could not attach to all threads for {pid}")
    return counters


def snapshot(pid, fds):
    processes = members(pid)
    hapi_pss = group_pss = wrapper_pss = 0
    agents = []
    for member in processes:
        kb = pss(member)
        group_pss += kb
        if member == pid:
            wrapper_pss = kb
        args = (PROC / str(member) / "cmdline").read_bytes().split(b"\0")
        if any(arg.endswith(b"/tier-b-claude.py") for arg in args):
            agents.append((member, processes[member]))
        else:
            hapi_pss += kb
    if len(agents) != 1:
        raise RuntimeError(f"expected one idle fake Claude in group {pid}, found {len(agents)}")
    proc = PROC / str(pid)
    start = time.monotonic_ns()
    fields = (proc / "stat").read_text().rsplit(") ", 1)[1].split()
    ticks = int(fields[11]) + int(fields[12])
    thread_dispatches = {}
    for task in (proc / "task").iterdir():
        try:
            key = (int(task.name), stat(task / "stat")[3])
            thread_dispatches[key] = int((task / "schedstat").read_text().split()[2])
        except (FileNotFoundError, ProcessLookupError):
            continue
    leader_dispatches = int((proc / "schedstat").read_text().split()[2])
    switches = sum(struct.unpack("Q", os.read(fd, 8))[0] for fd in fds)
    return dict(time=(start + time.monotonic_ns()) // 2, processes=processes,
                wrapper_pss_kib=wrapper_pss, hapi_pss_kib=hapi_pss, group_pss_kib=group_pss,
                agent=agents[0], helpers=len(processes)-2, ticks=ticks,
                leader_dispatches=leader_dispatches, threads=thread_dispatches, switches=switches)


def main():
    cohorts = json.loads(sys.argv[1])
    pids = [pid for group in cohorts.values() for pid in group["pids"]]
    if len(set(pids)) != 20:
        raise RuntimeError("expected twenty distinct wrappers")
    time.sleep(WARMUP_SECONDS)
    counters = switch_counters(pids)
    first = {pid: snapshot(pid, counters[pid]) for pid in pids}
    time.sleep(WINDOW_SECONDS)
    last = {pid: snapshot(pid, counters[pid]) for pid in pids}
    for fds in counters.values():
        for fd in fds:
            os.close(fd)
    for cohort in cohorts.values():
        samples = []
        for pid in cohort["pids"]:
            a, b = first[pid], last[pid]
            if a["processes"][pid] != b["processes"][pid] or a["agent"] != b["agent"]:
                raise RuntimeError(f"wrapper or agent generation changed for {pid}")
            ns = b["time"] - a["time"]
            observed = sum(count - a["threads"].get(t, 0) for t, count in b["threads"].items())
            samples.append(dict(pid=pid, elapsed_ns=ns, cpu_ticks=b["ticks"]-a["ticks"],
                                observed_thread_schedstat=observed,
                                retired_threads=len(a["threads"].keys() - b["threads"].keys()),
                                new_threads=len(b["threads"].keys() - a["threads"].keys()),
                                leader_schedstat=b["leader_dispatches"]-a["leader_dispatches"],
                                all_thread_switches=b["switches"]-a["switches"],
                                **{k:b[k] for k in ("wrapper_pss_kib","hapi_pss_kib","group_pss_kib","helpers")}))
        cohort["samples"] = samples
    result = dict(kernel=platform.release(), arch=platform.machine(), cpus=os.cpu_count(),
                  clk_tck=os.sysconf("SC_CLK_TCK"), warmup_seconds=WARMUP_SECONDS, cohorts=cohorts)
    Path(sys.argv[2]).write_text(json.dumps(result, indent=2) + "\n")


if __name__ == "__main__":
    main()
