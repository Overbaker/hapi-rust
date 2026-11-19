"""Measure the exact wrapper + one configured MCP helper per hosted root."""
import json
import os
import struct
import sys
import time

from sample import members, memory, thread_counters


def total(fds):
    return sum(struct.unpack("Q", os.read(fd, 8))[0] for fd in fds)


def main():
    wrapper, engine, *bridges = map(int, sys.argv[1:])
    owned = [wrapper, *bridges]
    identities = members(wrapper)
    if set(identities) != {*owned, engine}:
        raise ValueError(f"unexpected processes {identities}, expected {owned} and engine {engine}")
    for pid in bridges:
        if not os.path.samefile(f"/proc/{wrapper}/exe", f"/proc/{pid}/exe"):
            raise ValueError(f"bridge {pid} is not running wrapper executable")
    switches = thread_counters(owned, 0)
    clocks = thread_counters(owned, 1)
    start = time.monotonic_ns()
    before = {pid: (total(switches[pid]), total(clocks[pid])) for pid in owned}
    time.sleep(60)
    elapsed = time.monotonic_ns() - start
    samples = []
    for pid in owned:
        mem = memory(pid)
        samples.append(dict(pid=pid, role="wrapper" if pid == wrapper else "bridge",
                            rss=mem["Rss"], pss=mem["Pss"], uss=mem["USS"],
                            switches=total(switches[pid]) - before[pid][0],
                            cpu_ns=total(clocks[pid]) - before[pid][1]))
    if members(wrapper) != identities:
        raise ValueError("cohort changed during sample")
    print(json.dumps(dict(ns=elapsed, processes=samples, engine=memory(engine))))


if __name__ == "__main__":
    main()
