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
import ssl
import urllib.request
from pathlib import Path

PROC = Path("/proc")
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


def memory(pid):
    fields = {}
    for line in (PROC / str(pid) / "smaps_rollup").read_text().splitlines():
        key, _, value = line.partition(":")
        if key in ("Rss", "Pss", "Pss_Anon", "Pss_File", "Pss_Shmem", "Private_Clean",
                   "Private_Dirty", "Swap", "SwapPss"):
            fields[key] = int(value.split()[0])
    fields["USS"] = fields["Private_Clean"] + fields["Private_Dirty"]
    required = {"Rss","Pss","Pss_Anon","Pss_File","Swap","SwapPss","USS"}
    if not required <= fields.keys():
        raise RuntimeError(f"missing smaps fields for {pid}: {required - fields.keys()}")
    return fields


def thread_counters(pids, cpu_clock):
    source = Path(__file__).with_name("switch_counter.c")
    with tempfile.TemporaryDirectory() as temp:
        lib = Path(temp) / "switch_counter.so"
        subprocess.run(["cc", "-shared", "-fPIC", "-o", str(lib), str(source)], check=True)
        open_counter = ctypes.CDLL(str(lib), use_errno=True).open_thread_counter
    counters = {}
    for pid in pids:
        deadline = time.monotonic() + 3
        tasks = PROC / str(pid) / "task"
        while True:
            tids = {int(t.name) for t in tasks.iterdir()}
            fds = []
            for tid in tids:
                fd = open_counter(tid, cpu_clock)
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


def snapshot(pid, fds, clocks):
    processes = members(pid)
    hapi_pss = group_pss = wrapper_pss = 0
    agents = []
    wrapper = None
    for member in processes:
        mem = memory(member)
        kb = mem["Pss"]
        group_pss += kb
        if member == pid:
            wrapper_pss = kb
            wrapper = mem
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
            runtime, _, dispatches = map(int,(task / "schedstat").read_text().split())
            thread_dispatches[key] = (runtime, dispatches)
        except (FileNotFoundError, ProcessLookupError):
            continue
    leader_dispatches = int((proc / "schedstat").read_text().split()[2])
    switches = sum(struct.unpack("Q", os.read(fd, 8))[0] for fd in fds)
    cpu_ns = sum(struct.unpack("Q", os.read(fd, 8))[0] for fd in clocks)
    return dict(memory_kib=wrapper, cpu_ns=cpu_ns, time=(start + time.monotonic_ns()) // 2, processes=processes,
                wrapper_pss_kib=wrapper_pss, hapi_pss_kib=hapi_pss, group_pss_kib=group_pss,
                agent=agents[0], helpers=len(processes)-2, ticks=ticks,
                leader_dispatches=leader_dispatches, threads=thread_dispatches, switches=switches)


def active_sessions(cohorts):
    base = os.environ["HAPI_API_URL"]
    context = ssl.create_default_context()
    if base.startswith("https:"):
        context.load_verify_locations(os.environ["NODE_EXTRA_CA_CERTS"])
    result = {}
    for group in cohorts.values():
        for pid, sid in zip(group["pids"], group["ids"], strict=True):
            req = urllib.request.Request(base + "/api/sessions/" + sid, headers={
                "Authorization": "Bearer " + os.environ["HAPI_BENCH_WEB_TOKEN"]})
            with urllib.request.urlopen(req, context=context, timeout=10) as response:
                row = json.load(response)["session"]
            if not row["active"] or row["metadata"]["hostPid"] != pid:
                raise RuntimeError(f"session {sid} inactive or changed generation")
            result[pid] = row["activeAt"]
    return result


def sole_executable(pid):
    image = (PROC / str(pid) / "exe").stat()
    found = []
    for entry in PROC.iterdir():
        if entry.name.isdecimal():
            try:
                other = (entry / "exe").stat()
                if (other.st_dev, other.st_ino) == (image.st_dev, image.st_ino):
                    found.append(int(entry.name))
            except (PermissionError, ProcessLookupError, FileNotFoundError):
                continue
    if found != [pid]:
        raise RuntimeError(f"expected one resident executable, found {found}")
    return found


def main():
    cohorts = json.loads(sys.argv[1])
    warmup = int(sys.argv[3])
    pids = [pid for group in cohorts.values() for pid in group["pids"]]
    if len(set(pids)) != len(pids) or not pids:
        raise RuntimeError("expected distinct wrapper PIDs")
    active_sessions(cohorts)
    time.sleep(warmup)
    counters = thread_counters(pids, 0)
    clocks = thread_counters(pids, 1)
    single_start = sole_executable(pids[0]) if len(pids) == 1 else None
    start_ms = time.time_ns() // 1_000_000
    first = {pid: snapshot(pid, counters[pid], clocks[pid]) for pid in pids}
    time.sleep(WINDOW_SECONDS / 2)
    heartbeat = active_sessions(cohorts)
    time.sleep(WINDOW_SECONDS / 2)
    last = {pid: snapshot(pid, counters[pid], clocks[pid]) for pid in pids}
    end_ms = time.time_ns() // 1_000_000
    single_end = sole_executable(pids[0]) if len(pids) == 1 else None
    post = active_sessions(cohorts)
    for fds in (*counters.values(), *clocks.values()):
        for fd in fds:
            os.close(fd)
    for cohort in cohorts.values():
        samples = []
        for pid in cohort["pids"]:
            a, b = first[pid], last[pid]
            if a["processes"][pid] != b["processes"][pid] or a["agent"] != b["agent"]:
                raise RuntimeError(f"wrapper or agent generation changed for {pid}")
            alive = max((t for t in (heartbeat[pid],post[pid]) if start_ms <= t <= end_ms), default=None)
            if alive is None:
                raise RuntimeError(f"no in-window heartbeat for {pid}")
            observed = [sum(count[k] - a["threads"].get(t, (0,0))[k]
                            for t, count in b["threads"].items()) for k in (0,1)]
            samples.append(dict(pid=pid, elapsed_ns=b["time"]-a["time"], cpu_ticks=b["ticks"]-a["ticks"],
                                cpu_ns=b["cpu_ns"]-a["cpu_ns"], observed_schedstat_cpu_ns=observed[0],
                                observed_thread_schedstat=observed[1], active_at_ms=alive, active_after=True,
                                retired_threads=len(a["threads"].keys() - b["threads"].keys()),
                                new_threads=len(b["threads"].keys() - a["threads"].keys()),
                                leader_schedstat=b["leader_dispatches"]-a["leader_dispatches"],
                                all_thread_switches=b["switches"]-a["switches"],
                                **{k:b[k] for k in ("memory_kib","wrapper_pss_kib","hapi_pss_kib","group_pss_kib","helpers")}))
        cohort["samples"] = samples
    result = dict(kernel=platform.release(), arch=platform.machine(), cpus=os.cpu_count(),
                  window_start_ms=start_ms, window_end_ms=end_ms,
                  singleton_executable_pids_before=single_start, singleton_executable_pids_after=single_end,
                  clk_tck=os.sysconf("SC_CLK_TCK"), warmup_seconds=warmup, cohorts=cohorts)
    Path(sys.argv[2]).write_text(json.dumps(result, indent=2) + "\n")


if __name__ == "__main__":
    main()
