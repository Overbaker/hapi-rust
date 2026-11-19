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
        if env.get("round")!=3: continue
        assert env["gitCommit"]==commit==env["commit"]
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
assert len({a["harness"] for a in attempts})==1, "Cannot combine different acquisition sources"
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
    if not a["section"].startswith(("stream-", "stream:")): continue
    for r in a["rows"]:
        if r.get("kind") not in ("observer","stream","invalid"): continue
        scenario="observer" if r["kind"]=="observer" else r["scenario"]
        suite[scenario,r["kib"],r["repeat"]]=r
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
doc=["# Provisional Phase 0 ledger · aedas-me"]
put=doc.append
put(f"Executed in WSL on **aedas-me** ({host}), {e['cpu']}, {e['logicalCpus']} logical CPUs, {e['memoryBytes']:,} bytes RAM, {e['kernel']}. Measured Git commit (remote source digest verified against its Git blobs): `{commit}`. Each section identifies its benchmark source hash. Absolute thresholds below apply provisionally to this host. Confirmation on india-vps is outstanding for every gate.")
put(f"The full-suite invocation runs each acquisition section and generates the [measurement JSON](../../../../benchmarks/e2e-baseline/results/{json_name}), [combined JSONL]({raw_name}), and Markdown. Timestamped raw ledgers and compressed traces/profiles are in `benchmarks/e2e-baseline/results/`. Only results for this Git commit and its single verified harness digest supply these measurements.")
put('''```bash
revision=$(git rev-parse HEAD)
digest=$(python3 benchmarks/e2e-baseline/fingerprint.py "$revision")
/root/hapi-ci/aedas-me-verify.sh /root/hapi-e2e hapi-e2e "BASELINE_COMMIT=$revision BASELINE_EXPECTED_SHA256=$digest bun run bench:e2e-baseline"
```''')
put("## Provenance and coverage")
processor=hashlib.sha256(b"".join((Path(__file__).parent/f).read_bytes() for f in ("report.py","attribution.py"))).hexdigest()
put(f"Report/attribution source digest: `{processor}`. Measurement hashes below cover source files in both benchmark directories, excluding results. The launcher checks this digest before acquisition; history from other commits is excluded.")
put(table(["Remote raw file","Harness SHA-256","Playwright / Chromium / Bun","Sections (exit code)"],[
 (file,group[0]["harness"]," / ".join(group[0]["environment"][k] for k in ("playwright","chromium","bun")),
  ", ".join(f"{a['section']} ({a['exit']})" for a in group))
 for file in dict.fromkeys(a["file"] for a in attempts) if (group:=[a for a in attempts if a["file"]==file])]))
lcp_ok={rate:sum(r.get("kind")=="lcp" and r["rate"]==rate and r["mounted"]==51 for r in lcps)>=20 for rate in [1,4]}
put(table(["Requirement","Coverage"],[
 ("Three checkouts on india-vps","Not measured; one source tree on aedas-me"),
 ("500-message LCP, matched completed window",", ".join(f"{rate}× {status(valid)}" for rate,valid in lcp_ok.items())),("2,000-message scroll smoothness",status(scroll)+("" if scroll else " · "+reason("scroll"))),
 ("100 cumulative updates; frame work and long tasks",f"{len(streams)} accepted captures; invalid window/paint measurements are flagged below"),("Cold 200-row parse and normalize, two seeds",status(len(cold)>=80)),
 ("Hub page CPU, TTFB; SSE full/projected bytes; gzip load",status(pages and wire and len(loads)==4)),
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
    paints=[r for r in lcps if r.get("kind")=="lcp" and r["rate"]==rate]
    elements=", ".join(sorted({str(r.get("tag") or "unavailable")+(" in thread" if r.get("inThread") else " outside thread") for r in paints}))
    body.append((rate,len(rr),bad,elements or "—",span([r["size"] for r in paints]),span([r["mountedAtPaint"] for r in paints]),n(med(rr)) if len(rr)>=20 else "not measurable here",span(rr) if len(rr)>=20 else "—"))
put(table(["CPU ×","Accepted","Discarded","LCP element","Area px²","Rows at observer delivery","LCP median ms","Range ms"],body))
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
def grouped(rows):
    groups={}
    for r in rows:
        groups.setdefault((r.get("scenario","observer"),r["kib"]),[]).append(r)
    return groups

def spread(values):
    values=[v for v in values if v is not None]
    return f"{n(med(values))} ({span(values)})" if values else "not measurable here"

groups=grouped(streams)
focus={key:rr for key,rr in groups.items() if key[0] in ("baseline","assistant")}
put("Each table shows the median and min–max across independent turns. Percentiles are computed within each turn first. Five processes rotate size order; each turn uses a fresh context and its own seeded session. Frame summaries exclude captures without adequate frame coverage.")
body=[]
for (case,kib),rr in groups.items():
    peaks=[r["peakFrameMs"]["p95"] for r in rr if r["observedFrames"]>=100 and r.get("peakFrameMs")]
    body.append((case,kib,len(rr),rr[0]["before"],spread([1000*r["observedFrames"]/r["elapsedMs"] for r in rr]),
                 spread([100*r["busyMs"]/r["elapsedMs"] for r in rr]),spread([r["workMs"]["p50"] for r in rr]),spread([r["workMs"]["p95"] for r in rr]),
                 len(peaks),spread(peaks),spread([r["longTasks"] for r in rr])))
put(table(["Case","KiB","Turns","Mounted","fps","Busy %","Inter-update busy p50 ms","Busy p95 ms","Valid frame N","Peak frame work p95 ms","Long tasks"],body))
missing=[r for r in suite.values() if r.get("kind")=="invalid"]
if missing: put(table(["Rejected case","KiB","Repeat","Reason"],[(r["scenario"],r["kib"],r["repeat"],r["reason"]) for r in missing]))
put("The earlier single default-flags 30 KiB capture was an unexplained outlier. Use the repeated flags and software ranges above for comparison. Work depends on mounted rows: a freshly opened 20-message tail with six mounted rows is closer to rows5 than to the mixed 51-row baseline; its exact component mix differs.")
put("Main-thread work sums occupied RunTask spans between animation callbacks within each SSE window. A frame statistic requires ≥100 observed frames and a frame boundary in ≥80% of update windows. CPU sampling is on the renderer main thread.")
body=[]
for key,rr in focus.items():
    attrs=[r["attribution"] for r in rr]
    for level,bucket in [("long tasks","exclusiveLongMs"),("all occupied tasks","allOccupiedMs")]:
        body.append((*key,level,len(rr),*(spread([a[bucket].get(k,0)/a["updates"] for a in attrs]) for k in ("reactRender","reactCommit","layout","markdown","other"))))
put(table(["Path","KiB","Window","Turns","Render ms/update","Commit","Layout","Parser","Other"],body))
put("These phase buckets are exclusive. The all-occupied rows include assistant rendering outside long tasks. Long-task sampling and trace durations can be compared in the JSON.")
body=[]
for key,rr in focus.items():
    attrs=[r["attribution"] for r in rr]
    body.append((*key,spread([a["exactlyOne"] for a in attrs]),spread([a["onsetMedian"] for a in attrs]),
        *(spread([a["namedLongMs"].get(k,0)/a["updates"] for a in attrs]) for k in ("syncRoot","geometry","timestamps")),
        spread([a["markdownShare"] for a in attrs]),spread([a["subtreeShare"] for a in attrs]),
        spread([a["markdownPerUpdateP95"] for a in attrs]),spread([a["subtreePerUpdateP95"] for a in attrs])))
put(table(["Path","KiB","Updates with one long task","Onset ms","Sync root ms/update","Geometry","Timestamps","Parser %","Inclusive subtree %","Parser p95 ms/update","Subtree p95 ms/update"],body))
put("Parser and subtree columns count all occupied samples with a matching ancestor. Subtree includes the parser, mapped React Markdown components, react-markdown, hast-to-JSX, and Shiki. Inclusive values overlap React phase costs. Recognizable stacks are a lower bound on Markdown-related work; the entire subtree is not necessarily removable by WASM. Idle and unattributed program samples outside occupied tasks are excluded.")
put(table(["Observer-only reasoning KiB","Turns","Tasks >50 ms","Median duration ms","p95 ms"],[
    (k,len(rr),spread([len(r["durations"]) for r in rr]),spread([med(r["durations"]) for r in rr]),spread([quant(r["durations"],.95) for r in rr]))
    for (_,k),rr in grouped(observers).items()]))
put("Observer-only turns use PerformanceObserver without tracing, CPU profiling or per-update marks. The comparison across runs bounds instrumentation sensitivity empirically.")
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
put(table(["50 readers, ten streams","KiB","Deliveries","Emit → reader p99 ms","Hub CPU, % one core"],[(r['encoding'],r['kib'],r['deliveryMs']['n'],n(r['deliveryMs']['p99']),n(r['cpuPctOneCore'])) for r in loads]))
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
gzip_counts=next((r["handlerCpuSamples"] for r in profiles if r["encoding"]=="gzip"),{})
codec_share=100*(gzip_counts.get("parse",0)+gzip_counts.get("stringify",0))/sum(gzip_counts.values()) if sum(gzip_counts.values()) else None
kill_page=bool(gz) and max(gz)<50
kill_cpu=codec_share is not None and codec_share<30
gate("D-B","NO-GO" if kill_page or kill_cpu else "Undecided",f"Sequential gzip page p95 {span(gz)} ms (kill <50 ms: {kill_page}); sampled parse + stringify share {n(codec_share)}% (kill <30%: {kill_cpu}). Either disjunct suffices. Host-local, pending india-vps confirmation.")
term_deltas=[]
for run in [1,2,3]:
    pair={r["mode"]:r for r in term if r["run"]==run}
    if "append" in pair and "noop" in pair and all(pair[k]["hubP99"] is not None for k in ("append","noop")):
        term_deltas.append(pair["append"]["hubP99"]-pair["noop"]["hubP99"])
gate("D-C","Undecided","Kill if buffer-attributable p99 event-loop delay <20 ms. The earlier pipe-starvation result is withdrawn. Paced socket trials isolate direct append time and compare main-loop probes; three synthetic trials at one offered rate cannot establish production peak attribution. "+("Maximum individual append "+n(max(max(r['calls']) for r in term if r['mode']=='append'))+" ms." if any(r['mode']=='append' for r in term) else "No verified append trial.")+" Append-minus-noop hub p99 differences in ms: "+(", ".join(n(v) for v in term_deltas) or "not measurable here")+".")
gate("D-D layout","GO" if 1 in mounts else "Undecided",f"Target zero >50 ms main-thread tasks. Remove duplicate diffLines and slice/collapse DOM first. Both computations combined account for at most a median {n(mounts.get(1))}% of blocking at 1×.")
for k in ['distributed','high-edit','large']:
    v=classes.get(k,[])
    decision="NO-GO" if len(v)>=3 and max(v)<12 else "Undecided"
    gate("D-D WASM "+k,decision,f"Quiet JS Worker p95 {span(v)} ms ({len(v)} runs); <12 ms kills WASM. These are synthetic inputs. Even when ≥12 ms, a comparison is conditional on observing representative ≥500-line edits with comparable edit distance in authorized sessions. The recorded examples do not establish prevalence.")
de=[]
for (case,kib),rr in focus.items():
    attrs=[r["attribution"] for r in rr]
    parser=[a["markdownPerUpdateP95"] for a in attrs]
    subtree=[a["subtreePerUpdateP95"] for a in attrs]
    interpretation=("contradicted at this size by every pre-memo subtree p95" if max(subtree)<=12 else
                    "pre-memo subtree p95 exceeds 12 ms in every turn" if min(subtree)>12 else "pre-memo subtree p95 straddles 12 ms")
    de.append(f"{case} {kib} KiB (N={len(rr)}): median parser/subtree share {n(med([a['markdownShare'] for a in attrs]))}%/{n(med([a['subtreeShare'] for a in attrs]))}%; parser/subtree p95 {spread(parser)} / {spread(subtree)} ms; {interpretation}.")
gate("D-E","Undecided"," ".join(de)+" Design kill: post-memo frame-work p95 ≤12 ms. Proposed amendment: also require post-memo Markdown-attributed p95 >12 ms/update before a WASM comparison. These are pre-memo observations.")
four=[hydration[4,k] for k in ['standard','heavy'] if (4,k) in hydration]
gate("D-F","NO-GO" if len(four)==2 and max(four)<25 else "GO" if len(four)==2 and min(four)>=25 else "Undecided",f"200-row cold hydration 4× CPU p95 (standard / heavy): {' / '.join(n(x) for x in four)} ms. Kill below 25 ms; limited to these modeled response sizes.")
shell=next((r for r in hooks if r['hook']=='shell'),None)
shell_ms=[s*1000 for s in shell["seconds"]] if shell else []
shell_max=max(shell_ms) if len(shell_ms)==200 else None
gate("D-G","NO-GO" if shell_max is not None and shell_max<=10 else "Undecided",
     f"Shell maximum {n(shell_max)} ms; {sum(ms>10 for ms in shell_ms)}/{len(shell_ms)} runs above 10 ms. A maximum ≤10 ms meets the design kill for a Rust hook on this host. TS uses the source command; the shipped binary and a distinct resident alternative were not benchmarked.")
put(table(["Gate (aedas-me only)","Disposition","Evidence and criterion"],[(k,v['decision'],v['evidence']) for k,v in gates.items()]))
complete_load={(r["encoding"],r["kib"]) for r in loads}=={(enc,kib) for enc in ("gzip","identity") for kib in (6,30)}
worst_lag=max((r["deliveryMs"]["p99"] for r in loads),default=None)
worst_cpu=max((r["cpuPctOneCore"] for r in loads),default=None)
g2="NO-GO" if complete_load and worst_lag<20 and worst_cpu<25 else "Undecided"
detail=f"Worst of 6/30 KiB, identity/gzip: delivery p99 {n(worst_lag)} ms and {n(worst_cpu)}% of one core. Kill requires both p99 <20 ms and CPU <25%. Production peak and server-only broadcast p99 remain uncalibrated."
gate("G2",g2,detail)
put(f"G2: {g2} (synthetic load only). "+detail+" G1/G3 remain undecided without ten-session PSS. Agent startup, model first token, network RTT and DOM layout are outside the claimed savings.")
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
(dest/json_name).write_text(json.dumps(dict(gitCommit=commit,harness=e["harness"],processor=processor,attempts=[{k:v for k,v in a.items() if k not in ('rows','environment')} for a in attempts],
    decisions=gates,measurements=measurements),indent=2)+"\n")
