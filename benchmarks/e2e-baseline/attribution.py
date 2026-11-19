"""Attribute sampled renderer stacks inside traced tasks on the same monotonic clock."""
import bisect
import gzip
import json
import math
import statistics
from collections import Counter, defaultdict

def quant(values, p):
    return sorted(values)[max(0, math.ceil(len(values)*p)-1)] if values else None

def med_frame_gap(frames):
    return statistics.median((b-a)/1000 for a,b in zip(frames,frames[1:])) if len(frames)>1 else None

def analyze(root, row):
    def load(key):
        with gzip.open(root/row[key], "rt") as f:
            return json.load(f)
    trace, cpu = load("traceFile"), load("profileFile")
    starts = trace["chunks"]
    tasks = sorted((max(a,trace["start"]),min(b,trace["end"])) for a,b in trace["tasks"])
    task_starts = [a for a,b in tasks]
    layout = trace["layout"]
    layout_starts = [a for a,b in layout]
    delays = defaultdict(list)
    long_durations = []
    for a,b in tasks:
        if b-a <= 50000: continue
        long_durations.append((b-a)/1000)
        index = bisect.bisect_right(starts,a)-1
        if index >= 0: delays[index].append((a-starts[index])/1000)
    parents = {c:n["id"] for n in cpu["nodes"] for c in n.get("children",[])}
    nodes = {n["id"]:n for n in cpu["nodes"]}
    stacks = {}
    for index in nodes:
        cursor = index
        stack = []
        while cursor is not None:
            stack.append(nodes[cursor])
            cursor=parents.get(cursor)
        names = {n["frame"] for n in stack}
        labels = " ".join(names)
        categories = [n.get("category") for n in stack]
        category = "markdown" if "markdown" in categories else next((c for c in categories if c in ("reactRender","reactCommit")), "other")
        stacks[index] = (category,names, {
            "renderPhase": "reactRender" in categories, "commitPhase":"reactCommit" in categories,
            "syncRoot": "performSyncWorkOnRoot" in labels or "processRootScheduleInMicrotask" in labels,
            "geometry": "clientHeight" in labels or "useSizeHandle" in labels,
            "timestamps": "MessageTimestamp" in labels or "formatMessageTimestamp" in labels,
            "storage": "setItem" in labels})
    ts = cpu["startTime"]
    inclusive, exclusive, five, named_long, named_all = Counter(),Counter(),Counter(),Counter(),Counter()
    per_update = [0.] * len(starts)
    all_occupied=Counter()
    for node, dt in zip(cpu["samples"],cpu["timeDeltas"]):
        ts += dt
        if not trace["start"]<=ts<=trace["end"]: continue
        idx = bisect.bisect_right(task_starts,ts)-1
        if idx<0 or ts>tasks[idx][1]: continue
        ms=dt/1000
        category,names,named = stacks[node]
        l=bisect.bisect_right(layout_starts,ts)-1
        if l>=0 and ts<=layout[l][1]: category="layout"
        all_occupied[category]+=ms
        update=bisect.bisect_right(starts,ts)-1
        if category=="markdown" and update>=0: per_update[update]+=ms
        if tasks[idx][1]-tasks[idx][0]<=5000: continue
        five[category]+=ms
        for k,matched in named.items():
            if matched: named_all[k]+=ms
        if tasks[idx][1]-tasks[idx][0]<=50000: continue
        exclusive[category]+=ms
        for k,matched in named.items():
            if matched: named_long[k]+=ms
        for name in names:
            if name.strip() and not name.startswith("("): inclusive[name]+=ms
    return dict(maxTaskMs=max(((b-a)/1000 for a,b in tasks),default=0),
        medianFrameGapMs=med_frame_gap(trace["frames"]),updates=len(starts), longCount=len(long_durations), linkedUpdates=len(delays),
        exactlyOne=sum(len(v)==1 for v in delays.values()), onsetMedian=statistics.median(v[0] for v in delays.values()) if delays else None,
        durationMedian=statistics.median(long_durations) if long_durations else None,
        tracedLongMs=sum(long_durations), sampledLongMs=sum(exclusive.values()),
        exclusiveLongMs=dict(exclusive),namedLongMs=dict(named_long),namedOver5Ms=dict(named_all),
        topInclusiveLongMs=sorted(inclusive.items(), key=lambda entry: (-entry[1], entry[0]))[:12],over5Ms=dict(five),
        markdownShare=100*all_occupied["markdown"]/sum(all_occupied.values()) if sum(all_occupied.values()) else None,
        markdownPerUpdateP95=quant(per_update,.95))
