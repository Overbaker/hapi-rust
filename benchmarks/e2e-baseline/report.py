"""Rebuild the review ledger from remote measurements without hand-transcribed tables."""
import hashlib
import json
import statistics
import sys
from pathlib import Path
from attribution import analyze, quant

dest, commit, host = sys.argv[1:]
dest=Path(dest)
reports=Path("openspec/changes/rust-wasm-e2e-integration/reports")
reports.mkdir(parents=True,exist_ok=True)
stem=f"baseline-{commit}-{host}"
attempts, raw, selected = [], [], {}
for file in sorted(dest.glob(stem+"-*.jsonl")):
    lines=[json.loads(line) for line in file.read_text().splitlines()]
    env=lines[0]
    current=None
    for number,row in enumerate(lines,1):
        raw.append(dict(rawFile=file.name,line=number,**row))
        if env.get("round")!=2: continue
        if row.get("state")=="start":
            section=row["section"]
            current=dict(section=section,file=file.name,harness=env["harness"],environment=env,rows=[],exit=None)
            attempts.append(current)
            selected[section]=current
        elif "exit" in row:
            assert current and current["section"]==row["section"]
            current["exit"]=row["exit"]
            current=None
        elif current: current["rows"].append(row)
assert attempts
def get(section):
    a=selected.get(section)
    return a["rows"] if a and a["exit"]==0 else []
def record(section):
    return selected.get(section,{}).get("rows",[])
def n(x):
    return "—" if x is None else f"{x:,.2f}".rstrip("0").rstrip(".")
def med(xs): return statistics.median(xs) if xs else None
def span(xs): return f"{n(min(xs))}–{n(max(xs))}" if xs else "—"
def triplet(xs): return "/".join(n(quant(xs,p)) for p in (.5,.95,.99))
def table(labels,body):
    def line(cells): return "| "+" | ".join(str(v).replace("|","/") for v in cells)+" |"
    return "\n".join([line(labels),line(["---"]*len(labels)),*(line(r) for r in body)])
def status(valid): return "Measured" if valid else "Not measurable here"
def reason(section):
    rr=[r for r in record(section) if r.get("kind")=="invalid"]
    return rr[-1]["reason"] if rr else "no verified measurement"
nav=get("navigation")
paint_attempt=next((a for a in reversed(attempts) if a["section"] in ("navigation","lcp")),None)
lcps=paint_attempt["rows"] if paint_attempt and paint_attempt["exit"]==0 else []
cold=[r for r in nav if r.get("kind")=="cold"]
mount=[r for r in get("mount") if r.get("kind")=="mount"]
scroll=[r for r in get("scroll") if r.get("kind")=="scroll" and r.get("fps",0)>=30]
suite={}
for a in attempts:
    if a["section"]=="stream": suite={}
    elif a["section"].startswith("stream:"):
        suite={key:r for key,r in suite.items() if key[0]!=a["section"].split(":")[1]}
    else: continue
    if a["exit"]==0:
        for r in a["rows"]:
            if "scenario" in r: suite[r["scenario"],r.get("kib")]=r
            if r.get("kind")=="observer": suite["observer",r["kib"]]=r
streams=[r for r in suite.values() if r.get("kind")=="stream"]
observers=[r for r in suite.values() if r.get("kind")=="observer"]
for r in streams:
    target=int(r["scenario"][4:]) if r["scenario"].startswith("rows") else 51
    assert r["before"]==target and r["updates"]==(100 if r["kib"] else 0)
    r["attribution"]=analyze(dest,r)
hub=get("hub")
wire=[r for r in hub if "modeledDeltaGzip" in r]
pages=[r for r in hub if "pageMs" in r]
loads=[r for r in hub if "subscribers" in r]
profiles=[r for r in hub if "handlerCpuSamples" in r]
term=[dict(run=i,**r) for i in range(1,4) for r in get(f"flood-{i}") if r.get("kind")=="terminal"]
workers=[dict(run=i,**r) for i in range(1,5) for r in get(f"diff-{i}") if r.get("kind")=="worker"]
hooks=get("hook")
edits=get("edits")
e=attempts[-1]["environment"]
json_name=stem+".json"
raw_name=stem+".jsonl"
doc=["# Partial aedas-me ledger · Phase 0 final review"]
put=doc.append
put(f"Executed in WSL on **aedas-me** ({host}), {e['cpu']}, {e['logicalCpus']} logical CPUs, {e['memoryBytes']:,} bytes RAM, {e['kernel']}. Source checkout HEAD supplied by the caller: `{commit}`. Each section identifies its benchmark source hash. Absolute thresholds below apply provisionally to this host. Confirmation on india-vps is outstanding for every gate.")
put(f"One remote payload generates the [measurement JSON](../../../../benchmarks/e2e-baseline/results/{json_name}), [combined JSONL]({raw_name}), and Markdown. Timestamped raw ledgers and compressed traces/profiles are in `benchmarks/e2e-baseline/results/`. The combined archive preserves earlier review rounds; only round-2 attempts supply current measurements.")
put('''```bash
/root/hapi-ci/aedas-me-verify.sh /root/hapi-e2e hapi-e2e "BASELINE_COMMIT=$(git rev-parse HEAD) bun run bench:e2e-baseline"
```''')
put("## Provenance and coverage")
processor=hashlib.sha256(b"".join((Path(__file__).parent/f).read_bytes() for f in ("report.py","attribution.py"))).hexdigest()
put(f"Report/attribution source digest: `{processor}`. Measurement hashes below cover source files in both benchmark directories, excluding results. Earlier unversioned round-1 rows are archived, never selected as current evidence.")
put(table(["Remote raw file","Harness SHA-256","Playwright / Chromium / Bun","Sections (exit code)"],[
 (file,group[0]["harness"]," / ".join(group[0]["environment"][k] for k in ("playwright","chromium","bun")),
  ", ".join(f"{a['section']} ({a['exit']})" for a in group))
 for file in dict.fromkeys(a["file"] for a in attempts) if (group:=[a for a in attempts if a["file"]==file])]))
lcp_ok={rate:sum(r.get("kind")=="lcp" and r["rate"]==rate and r["mounted"]==51 for r in lcps)>=20 for rate in [1,4]}
put(table(["Requirement","Coverage"],[
 ("Three checkouts on india-vps","Not measured; one source tree on aedas-me"),
 ("500-message LCP, matched completed window",", ".join(f"{rate}× {status(valid)}" for rate,valid in lcp_ok.items())),("2,000-message scroll smoothness",status(scroll)+("" if scroll else " · "+reason("scroll"))),
 ("100 cumulative updates; frame work and long tasks",f"{len(streams)} accepted captures; invalid window/paint measurements are flagged below"),("Cold 200-row parse and normalize, two seeds",status(len(cold)>=80)),
 ("Hub page CPU, TTFB; SSE full/projected bytes; gzip load",status(pages and wire and len(loads)==2)),
 ("Terminal append and hub lag",status(len(term)==9)+"; probe validity reported per trial"),
 ("Diff mount, Worker, hook cold start",status(all(sum(r["rate"]==rate for r in mount)>=10 for rate in (1,4)) and len(workers)>=9 and len(hooks)==2)),
 ("10-session PSS including children; prompt → first model token","Not measured; no live model or ten-agent stack"),
 ("Rust database safety","No Rust component accesses the disposable TS database")]))
put("## Browser end-to-end")
put("The final LCP probe warms a same-origin blank page, then opens the authentic cold route in a fresh browser context for each navigation. Six consecutive animation callbacks at exactly 51 mounted rows verify the common end state. Samples at other counts or without a verified paint are discarded. At least 20 accepted opens per CPU rate are required.")
body=[]
for rate in [1,4]:
    rr=[r["ms"] for r in lcps if r.get("kind")=="lcp" and r["rate"]==rate and r["mounted"]==51]
    bad=sum(r.get("metric")=="lcp" and r["rate"]==rate for r in lcps)
    body.append((rate,len(rr),bad,n(med(rr)) if len(rr)>=20 else "not measurable here",span(rr) if len(rr)>=20 else "—"))
put(table(["CPU ×","Accepted","Discarded","LCP median ms","Range ms"],body))
put("The hydration stress probe forces a fresh 200-row latest response and verifies every ID reached the real normalizer. This differs from normal navigation's initial 20-row tail and subsequent context loading. The heavy-tail model alternates Read/Bash tool records with 5/16/32/50 Ki-character outputs plus the existing truncation cases. These synthetic distributions have no production-frequency calibration.")
body=[]; hydration={}
for rate in [1,4]:
    for seed in ["standard","heavy"]:
        rr=[r for r in cold if r["rate"]==rate and r["seed"]==seed]
        if len(rr)<20: continue
        v=[r["ms"] for r in rr]
        hydration[rate,seed]=quant(v,.95)
        body.append((seed,rate,len(rr),n(med([r["responseBytes"]/200 for r in rr])),triplet(v)))
put(table(["Seed","CPU ×","Cold N","UTF-8 bytes / response row","Parse + normalize p50/p95/p99 ms"],body))
if scroll:
    for r in scroll:
        put(f"Validated scroll: {r['iterations']} wheel iterations, {n(r['elapsedMs'])} ms elapsed vs {n(r['expectedMs'])} ms scheduled, {n(r['fps'])} observed fps, {n(r['distance'])} px travelled, {r['requests']} older responses. Main-thread frame work p95 {n(r['frameWorkMs']['p95'])} ms; {r['longTasks']} long tasks and {r['droppedFrameEvents']} compositor drop events.")
else:
    put("**Scroll smoothness: not measurable here.** Acceptance requires a warmed compositor, ≥30 observed fps, adequate travel, an older-page response, and elapsed time within 0.8–1.5× the wheel schedule including settlement. Rejected trace counts and timings are diagnostics only.")
put("### Streaming and attribution")
put("Each comparison starts in a pristine seeded session. Baseline and controls require 51 mounted rows immediately before delivery. The size sweep uses homogeneous alternating user/assistant records. Reasoning snapshots render in the thinking card; the assistant case sends message + streamSnapshot through the assistant-text renderer. All use 100 cumulative socket updates at 250 ms spacing. Idle uses a 25-second window. Default-flags omits both software compositor overrides.")
body=[]
for r in streams:
    valid=r["observedFrames"]>=100 and r.get("peakFrameMs")
    body.append((r["scenario"],r["kib"],f"{r['before']} → {r['mounted']}",r["observedFrames"],n(r["observedFrames"]*1000/r["elapsedMs"]),n(100*r["busyMs"]/r["elapsedMs"]),n(r["workMs"]["p95"]),triplet(r["peakFramePerUpdate"]) if valid else "not measurable here",r["longTasks"]))
for r in suite.values():
    if r.get("kind")=="invalid": body.append((r["scenario"],r["kib"],f"expected {r.get('expected')}, observed {r.get('actual')}","—","—","—","—","not measurable here","—"))
put(table(["Case","KiB","Mounted before → after","Observed frames","fps","Main busy %","Inter-delivery busy p95 ms","Peak occupied work / update p50/p95/p99 ms","Tasks >50 ms"],body))
put("Main-thread work sums occupied RunTask spans between animation callbacks within each SSE update window. At least 80% of windows must contain a frame boundary. A stalled headless compositor invalidates frame timing. The earlier unmatched 4× run had five mounted rows against 51; its apparent acceleration does not estimate throttling.")
for r in streams:
    if r["scenario"]=="idle":
        put(f"Idle control: longest traced main-thread task {n(r['attribution']['maxTaskMs'])} ms and median observed frame gap {n(r['attribution']['medianFrameGapMs'])} ms.")
sweep=sorted([r for r in streams if r["scenario"].startswith("rows")],key=lambda r:r["before"])
if len(sweep)==3:
    put("With identical 6 KiB reasoning input and the homogeneous fixture, delivery-window busy p95 was "+
        ", ".join(f"{n(r['workMs']['p95'])} ms at {r['before']} rows" for r in sweep)+
        ". Compare row counts within this sweep; the mixed 51-row seed has a different component mix.")
focus=[r for r in streams if r["scenario"] in ("baseline","assistant")]
body=[]
for r in focus:
    a=r["attribution"]; count=a["updates"]
    body.append((r["scenario"],r["kib"],f"{a['exactlyOne']}/{count}",n(a["onsetMedian"]),n(a["durationMedian"]),n(a["tracedLongMs"]),n(a["sampledLongMs"]),*(n(a["exclusiveLongMs"].get(k,0)/count) for k in ["reactRender","reactCommit","layout","markdown","other"])))
put(table(["Path","KiB","Updates with exactly one long task","Median onset after SSE ms","Median long task ms","Traced long-task ms","Sampled long-task ms","Render ms/update","Commit","Style/layout","Markdown","Other"],body))
body=[]
for r in focus:
    a=r["attribution"]; count=a["updates"]
    per=lambda group,k:n(a[group].get(k,0)/count)
    body.append((r["scenario"],r["kib"],*(per("namedLongMs",k) for k in ("syncRoot","renderPhase","commitPhase","geometry","timestamps","storage")),per("namedOver5Ms","storage"),n(a["markdownShare"]),n(a["markdownPerUpdateP95"])))
put(table(["Path","KiB","Sync React root ms/update","Render stack","Commit stack","useSizeHandle/clientHeight","Timestamp formatting","setItem in long tasks","setItem in tasks >5 ms","Markdown % of sampled occupied time","Markdown ms/update p95, all occupied tasks"],body))
put("The first table uses exclusive buckets; the second uses inclusive stacks and can overlap render, commit and forced layout. Render/commit attribution uses samples in traced long tasks. The storage comparison also includes tasks above 5 ms; the parser-only estimate includes all occupied tasks. Background (program) samples outside those tasks are excluded. Sync-root samples contain processRootScheduleInMicrotask or performSyncWorkOnRoot. The phase stacks locate a synchronous React pass through the mounted thread after delivery. Sampling cannot count invocations of every row. Markdown-only improvements cannot remove other rendering, storage, and layout costs.")
reasoning=[r for r in focus if r["scenario"]=="baseline"]
if len(reasoning)==2:
    put("For the 6 / 30 KiB reasoning turns, "+ " / ".join(str(r["attribution"]["exactlyOne"]) for r in reasoning)+
        " of 100 updates had exactly one long task. Median task duration was "+
        " / ".join(n(r["attribution"]["durationMedian"]) for r in reasoning)+" ms. "+
        "Compare these with the observer-only durations below. The next TS experiment should bound the rows processed by this pass and measure the resulting timestamp formatting and synchronous viewport geometry reads. A parser change cannot eliminate either cost.")
for r in focus:
    a=r["attribution"]
    put(f"**{r['scenario']} {r['kib']} KiB** · highest inclusive frames inside long tasks: "+", ".join(f"`{name}` {n(ms/a['updates'])} ms/update" for name,ms in a["topInclusiveLongMs"][:5])+".")
put(table(["Observer-only reasoning","Mounted","Count >50 ms","Median duration ms","p95 ms"],[
 (r["kib"],r["before"],len(r["durations"]),n(med(r["durations"])),n(quant(r["durations"],.95))) for r in observers]))
for r in observers:
    match=next((f for f in focus if f["scenario"]=="baseline" and f["kib"]==r["kib"]),None)
    if match and r["durations"] and match["attribution"]["durationMedian"]:
        diff=match["attribution"]["durationMedian"]-med(r["durations"])
        put(f"{r['kib']} KiB traced minus observer-only median long-task duration: {n(diff)} ms ({match['longTasks']} vs {len(r['durations'])} tasks).")
put("The observer-only control runs the same replay without CDP tracing, profiling or per-update performance marks. Compare its duration and count to the traced reasoning case. This is an empirical overhead comparison on separate turns.")
put("## Hub end-to-end")
captures=[r for r in hub if r.get("kind")=="recordedTranscript"]
for r in captures:
    put(f"BASELINE_CLAUDE_JSONL replayed {r['assistantRecords']} recorded visible Claude messages ({r['textBytes']} text bytes) through the CLI handler and SSE route: {r['wireBytes']} frame bytes, {r['gzip']} gzip bytes. The default source is the repository's recorded Claude JSONL fixture. It contains settled messages and cannot establish a 6/30 KiB cumulative-turn ratio.")
put("The long-turn replay uses the checked-in prose/code corpus unless an authorized transcript supplies ≥30 KiB. Its 100 slices have synthetic boundaries. Actual SSE snapshots and projected deltas pass through the same production gzip transformer with per-frame flushing. Projected deltas have no implemented hub CPU path.")
put(table(["KiB","Full raw/gzip B","Projected delta + full settle raw/gzip B","Projected delta-only raw/gzip B","Full-settle / delta-only savings"],[
 (r['kib'],f"{r['bytes']}/{r['gzip']}",f"{r['modeledDeltaBytes']}/{r['modeledDeltaGzip']}",f"{r['deltaOnlyBytes']}/{r['deltaOnlyGzip']}",f"{n(r['gzip']/r['modeledDeltaGzip'])}× / {n(r['gzip']/r['deltaOnlyGzip'])}×") for r in wire]))
put(table(["Seed","Rows stored/returned","Encoding","TTFB p95 ms","Page p95 ms","Encoded bytes","Hub CPU ms/page"],[(r['seed'],f"{r['session']}/{r['rows']}",r['encoding'],n(r['ttfb']['p95']),n(r['pageMs']['p95']),r.get('wireBytes','—'),n(r.get('hubCpuMsPerPage'))) for r in pages]))
put(table(["Encoding","Decode","Parse","Stringify","Compression stack","Other route"],[(r["encoding"],*(f"{r['handlerCpuSamples'][k]} ({n(100*r['handlerCpuSamples'][k]/max(1,sum(r['handlerCpuSamples'].values())))}%)" for k in ["decode","parse","stringify","compression","other"])) for r in profiles]))
put("Page latency ends after reading the encoded body. The client then decompresses and validates it. CPU buckets count samples with identifiable page-handler or compression stacks. Async native compressor work can lack a JS stack; a zero compression bucket cannot establish zero cost.")
put(table(["50 readers, ten streams","Deliveries","Emit → reader p99 ms","Hub CPU, % one core"],[(r['encoding'],r['deliveryMs']['n'],n(r['deliveryMs']['p99']),n(r['cpuPctOneCore'])) for r in loads]))
put("Gzip subscribers negotiate compression and decompress each continuous stream in the reader process. The 83 ms producer interval is a synthetic 3× ACP rate.")
put("## Isolated measurements")
put("Terminal trials send 3,200 independent 64 KiB CLI terminal:output events spaced by at least 8 ms (target ceiling 7.81 MiB/s). A real authenticated terminal verifies all 200 MiB. The disposable hub times the production append call or a matched no-op at the same handler location. A worker independently pings the hub main loop every 5 ms and times the round trip. The in-hub monitorEventLoopDelay histogram and worker are checked against a deliberate 45 ms stall before each process's trials; an unresponsive probe is reported as not measurable here.")
put(table(["Run","Mode","MiB/s","Append calls","Call p95/p99/max ms","Hub ticks","Hub lag p99 ms","Worker ticks","Worker RTT p99 ms"],[
 (r['run'],r['mode'],n(r['receivedBytes']/1048576/(r['elapsedMs']/1000)),len(r['calls']),
  "/".join(n(v) for v in [r['append']['p95'],r['append']['p99'],max(r['calls'])]) if r['append'] else "—",
  r['ticks'],n(r['hubP99']) if r['hubP99'] is not None else "not measurable here",r['workerTicks'],n(r['workerRTT']['p99']) if r['workerRTT'] else "not measurable here") for r in term]))
mounts={}
body=[]
for rate in [1,4]:
    rr=[r for r in mount if r['rate']==rate]
    if len(rr)<10: continue
    times=[r['mount']['mountMs'] for r in rr]
    layout=[r['layoutMs'] for r in rr]
    fractions=[100*sum(r['mount']['diffMs'])/(r['mount']['mountMs']+r['mount']['forcedLayoutMs']) for r in rr]
    mounts[rate]=med(fractions)
    body.append((rate,len(rr),n(med(times)),span(times),n(med(layout)),span(layout),n(med(fractions)),n(med([r['mount']['nodes'] for r in rr]))))
put(table(["Real DiffView CPU ×","Fresh N","Mount median ms","Mount range","Layout median ms","Layout range","Ideal Worker upper bound %","DOM nodes"],body))
put("The upper bound divides both diffLines calls by synchronous mount plus forced layout. It assumes zero transfer or scheduling cost. One of those calls duplicates the other.")
classes={}
for r in workers:
    if r['runIdle']>=.90: classes.setdefault(r['workload'],[]).append(r['ms']['p95'])
put(table(["Worker workload","Process","Lines","Replacement spacing","Preflight / trial guest idle %","p95 compute ms","Quiet"],[
 (r['workload'],r['run'],r['lines'],r['stride'],f"{n(100*r['preflightIdle'])}/{n(100*r['runIdle'])}",n(r['ms']['p95']),r['runIdle']>=.90) for r in workers]))
put("Four fresh sequential Worker processes include a repeat of the initial process. All are archived. Only ≥90% guest idle before and during the trial qualifies for the per-class verdict. No passing run is removed for having an inconvenient timing.")
put(table(["Recorded edit source","Old → new bytes","Old → new lines"],[(r['source'],f"{r['oldBytes']} → {r['newBytes']}",f"{r['oldLines']} → {r['newLines']}") for r in edits]))
put("These captured Edit examples bound the observed sample only; they cannot estimate production workload prevalence.")
put(table(["Hook command","Fresh processes","p50/p95/p99 ms"],[(r['hook'],r['ms']['n'],"/".join(n(r['ms'][k]) for k in ('p50','p95','p99'))) for r in hooks]))
put("## Decisions")
gates={}
def gate(key,decision,detail): gates[key]=dict(decision=decision,evidence=detail)
ratios="; ".join(f"{r['kib']} KiB {n(r['gzip']/r['modeledDeltaGzip'])}×/{n(r['gzip']/r['deltaOnlyGzip'])}×" for r in wire)
gate("D-A","Undecided",f"Projected full-settle/delta-only savings: {ratios}. Kill requires <2× AND <30% hub CPU reduction. A ≥2× projection does not meet the byte conjunct; a <2× result needs the missing matched CPU measurement. The short real capture cannot resolve the long-turn gate.")
gz=[r['pageMs']['p95'] for r in pages if r['encoding']=='gzip']
gate("D-B","NO-GO" if gz and max(gz)<50 else "Undecided",f"Gzip page p95 {span(gz)} ms. Kill if p95 <50 ms OR parse + stringify <30%. Host-local disposition, pending india-vps confirmation.")
term_deltas=[]
for run in [1,2,3]:
    pair={r["mode"]:r for r in term if r["run"]==run}
    if "append" in pair and "noop" in pair and all(pair[k]["hubP99"] is not None for k in ("append","noop")):
        term_deltas.append(pair["append"]["hubP99"]-pair["noop"]["hubP99"])
gate("D-C","Undecided","Kill if buffer-attributable p99 event-loop delay <20 ms. The earlier pipe-starvation result is withdrawn. Paced socket trials isolate direct append time and compare main-loop probes; three synthetic trials at one offered rate cannot establish production peak attribution. "+("Maximum individual append "+n(max(max(r['calls']) for r in term if r['mode']=='append'))+" ms." if any(r['mode']=='append' for r in term) else "No verified append trial.")+" Append-minus-noop hub p99 differences in ms: "+(", ".join(n(v) for v in term_deltas) or "not measurable here")+".")
gate("D-D layout","GO" if 1 in mounts else "Undecided",f"Target zero >50 ms main-thread tasks. Remove duplicate diffLines and slice/collapse DOM first. Both computations combined account for at most a median {n(mounts.get(1))}% of blocking at 1×.")
for k in ['distributed','high-edit','large']:
    v=classes.get(k,[])
    decision="Undecided" if len(v)<3 or min(v)<12<=max(v) else "GO" if min(v)>=12 else "NO-GO"
    gate("D-D WASM "+k,decision,f"Quiet Worker p95 {span(v)} ms ({len(v)} runs). JS p95 <12 ms kills a WASM comparison for this class. GO qualifies a comparison only.")
gate("D-E","Undecided","The reasoning-card premise for Markdown WASM is contradicted: the earlier profile put Markdown at only about 2–8% of busy work, even assuming a free parser. Round-2 attribution and the assistant-text path are above. Require post-memo Markdown-attributed p95 >12 ms/update for a parser comparison; total frame time alone cannot qualify WASM. No post-memo path exists.")
four=[hydration[4,k] for k in ['standard','heavy'] if (4,k) in hydration]
gate("D-F","NO-GO" if len(four)==2 and max(four)<25 else "GO" if len(four)==2 and min(four)>=25 else "Undecided",f"200-row cold hydration 4× CPU p95 (standard / heavy): {' / '.join(n(x) for x in four)} ms. Kill below 25 ms; limited to these modeled response sizes.")
shell=next((r for r in hooks if r['hook']=='shell'),None)
gate("D-G","Undecided",f"Shell p50/p95 {n(shell['ms']['p50']) if shell else '—'}/{n(shell['ms']['p95']) if shell else '—'} ms. Kill at ≤10 ms; the design does not specify the statistic. Source TS command, no shipped binary or independent resident-alternative comparison.")
put(table(["Gate (aedas-me only)","Disposition","Evidence and criterion"],[(k,v['decision'],v['evidence']) for k,v in gates.items()]))
gzload=next((r for r in loads if r['encoding']=='gzip'),None)
g2="NO-GO (provisional 不实施 on synthetic 3×)" if gzload and gzload['deliveryMs']['p99']<20 and gzload['cpuPctOneCore']<25 else "Undecided (production peak is uncalibrated)"
put("G2: "+g2+". Production peak and server-only broadcast p99 are uncalibrated. "+(f"Gzip delivery p99 {n(gzload['deliveryMs']['p99'])} ms and CPU {n(gzload['cpuPctOneCore'])}% versus 20 ms / 25% of one core." if gzload else "Gzip load unavailable.")+" G1/G3 remain undecided without ten-session PSS. The report assigns no Rust savings to agent memory/startup, model first token, RTT or DOM layout.")
reports.mkdir(exist_ok=True)
(reports/"baseline.md").write_text("\n\n".join(doc)+"\n")
(reports/raw_name).write_text("".join(json.dumps(r,separators=(",",":"))+"\n" for r in raw))
skip={"calls","timeDeltas","samples","warmups","warmup","wall","headersMs","seconds","times","layoutIntervals","updateWork","peakFramePerUpdate"}
def compact_rows(group):
    return [{k:v for k,v in r.items() if k not in skip} for r in group]
measurements=dict(lcp=compact_rows([r for r in lcps if r.get("kind")=="lcp" and lcp_ok[r["rate"]]]) or None,
    hydration=compact_rows(cold),scroll=compact_rows(scroll) or None,diffMount=compact_rows(mount),
    streams=compact_rows(streams),observerOnly=compact_rows(observers),
    rejectedWindows=compact_rows([r for r in suite.values() if r.get("kind")=="invalid"]),
    page=compact_rows(pages),pageProfile=profiles,sseWire=wire,load=compact_rows(loads),terminal=compact_rows(term),
    capturedTranscript=captures,realEdits=edits,worker=compact_rows(workers),hooks=compact_rows(hooks))
(dest/json_name).write_text(json.dumps(dict(processor=processor,attempts=[{k:v for k,v in a.items() if k not in ('rows','environment')} for a in attempts],
    decisions=gates,measurements=measurements),indent=2)+"\n")
