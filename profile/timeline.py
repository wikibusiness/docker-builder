#!/usr/bin/env python3
"""Draws an imgbuild job's timeline from its log and prints what the machine did in every phase.

Needs IMGBUILD_PROFILE=1 (the imgbuild process's own samples) and IMGBUILD_SAMPLE=<image substring | all> (the node view) in the job; both come from prof.rs.

  timeline.py --log job.log -o out.png
  timeline.py --repo owner/repo --run 123456 --job 'build' -o out.png

Writes out.png (up to 10 panels on one time axis) and out.txt (per-phase table: CPU and disk per process, I/O wait, D-state wait functions).
Requires matplotlib and numpy.
"""
import argparse, collections, datetime as dt, json, re, statistics, subprocess, sys
import matplotlib; matplotlib.use("Agg")
import matplotlib.pyplot as plt
import numpy as np

TS = re.compile(r"^(\d{4}-\d\d-\d\dT[\d:.]+)Z ")
LANES = [
    ("plan, cache lookups", "#c9d6df", lambda c, l: c in ("plan", "cache-lookup")),
    ("docker load / create / login", "#f6d55c", lambda c, l: c.startswith("run-") and c not in ("run-exec", "run-layer")),
    ("RUN executes in container", "#d62728", lambda c, l: c == "run-exec"),
    ("RUN result read back", "#f28e2b", lambda c, l: c == "run-layer"),
    ("COPY / ADD / layer write", "#8fb8de", lambda c, l: c in ("layer-write", "copy-delta", "copy-entries", "extract", "copy-same")),
    ("push blobs", "#2f6db5", lambda c, l: c in ("push-blob", "push-layers")),
    ("push manifests", "#1b3f73", lambda c, l: c in ("push-manifest", "push-list")),
    ("seed wait", "#7b3fa0", lambda c, l: c == "seed-wait"),
]
KERNEL = ("kworker", "jbd2", "dmcrypt", "kcompactd", "kswapd", "ksoftirqd", "migration", "rcu", "md", "flush", "kthreadd", "irq")
DOCKER = ("dockerd", "containerd", "containerd-shim", "runc", "docker", "docker-proxy")
UNIT = {"cpu": "cores", "w": "MB/s"}


def fetch(run, job, host, repo):
    jobs = json.loads(subprocess.check_output(["gh", "run", "view", str(run), "--repo", f"{host}/{repo}", "--json", "jobs"]))["jobs"]
    j = next(j for j in jobs if re.search(job, j["name"]))
    return subprocess.check_output(["gh", "api", "--hostname", host, f"repos/{repo}/actions/jobs/{j['databaseId']}/logs"]).decode(errors="ignore")


def parse(text):
    prof, res, samp, events, end, wall = [], [], [], [], None, None
    for raw in text.splitlines():
        m = TS.match(raw)
        ts = dt.datetime.fromisoformat(m.group(1)) if m else None
        l = raw[m.end():] if m else raw
        if l.startswith("samp "):
            t, rest = l[5:].split(" ", 1)
            if t.replace(".", "", 1).isdigit():
                samp.append((float(t), rest))
        elif l.startswith("prof "):
            p = l.split(" ", 5)
            if re.match(r"[\d.]+$", p[4] if len(p) > 4 else ""):  # `prof <start> <dur> <wrMB> <cpu s> <rx MB> <tx MB> <cat> <label>`
                p = l.split(" ", 8)
                prof.append((float(p[1]), float(p[2]), float(p[3]), p[7], p[8] if len(p) > 8 else ""))
                res.append((float(p[4]), float(p[5]), float(p[6])))
            else:
                prof.append((float(p[1]), float(p[2]), float(p[3]), p[4], p[5] if len(p) > 5 else ""))
                res.append((None, None, None))
        elif l.startswith("profile: wall"):
            wall, end = float(re.search(r"wall ([\d.]+)s", l).group(1)), ts
        elif ts and re.match(r"#\d+ [\d.]+\s+\S", l):
            events.append((ts, l.split(None, 2)[2].strip()))
    start = end - dt.timedelta(seconds=wall) if end else None
    ev = [((ts - start).total_seconds(), e) for ts, e in events] if start else []
    return prof, res, samp, ev, wall


class Data:
    def __init__(self, samp, text):
        self.cpu, self.node, self.procs, self.dstate, self.cmd, self.ppid = [], [], [], [], {}, {}
        self.mem, self.dev, self.writes, self.own, self.pod, self.podcmd = [], collections.defaultdict(list), [], [], [], {}
        self.cgio, self.cancel = [], []
        ts = sorted({t for t, r in samp if r.startswith("n ")})
        self.dt = statistics.median(np.diff(ts)) if len(ts) > 2 else 0.5
        for t, r in samp:
            k = r[0]
            f = r.split()
            if k in "oqy" and r[:2] in ("o ", "q ", "y "):  # from the imgbuild process itself: 0.4 s ticks of its own
                if k == "o":
                    self.own.append((t - 0.2, [float(x) for x in f[1:]]))
                elif k == "q":
                    self.pod.append((t - 0.2, f[1], f[2], f[3], (int(f[4]) + int(f[5])) / 100, float(f[6]), float(f[7]), f[8], f[9] if len(f) > 9 else ""))
                else:
                    cl = " ".join(f[4:])
                    me = cl.endswith("(this process)")
                    self.podcmd[f[1]] = (f[3], f[2], cl[: -len(" (this process)")] if me else cl, me)
                continue
            t -= self.dt / 2
            if k == "c" and r[:2] == "c ":
                self.cpu.append((t, [int(x) for x in f[1:]]))
            elif k == "n" and r[:2] == "n ":
                self.node.append((t, [float(x) for x in f[1:]]))
            elif k == "m" and r[:2] == "m ":
                self.mem.append((t, [float(x) for x in f[1:]]))
            elif k == "k" and r[:2] == "k ":
                self.dev[f[1]].append((t, [float(x) for x in f[2:]]))
            elif k == "g" and r[:2] == "g ":
                self.cgio.append((t, float(f[1]), float(f[2]), float(f[3]), " ".join(f[4:])))
            elif k == "w" and r[:2] == "w ":
                files = []
                for part in " ".join(f[5:]).split(" ;; "):
                    path, _, mb = part.rpartition("=")
                    if path:
                        files.append((path, float(mb)))
                self.writes.append((t, f[1], f[2], f[3], float(f[4]), files))
            elif k == "p" and r[:2] == "p ":
                _, pid, comm, cg, u, s, rd, wr, state = f[:9]
                self.procs.append((t, pid, comm, cg, (int(u) + int(s)) / 100, float(rd), float(wr), state))
                if len(f) > 9:
                    self.cancel.append((t, pid, comm, cg, float(f[9])))
            elif k == "d" and r[:2] == "d ":
                self.dstate.append((t, f[1], f[2], f[3], f[4] if len(f) > 4 else "?"))
            elif k == "x" and r[:2] == "x ":
                rest = f[4:]
                if rest and rest[0].startswith("ppid="):
                    self.ppid[f[1]] = rest[0][5:]
                    rest = rest[1:]
                self.cmd[(f[1])] = (f[2], f[3], " ".join(rest))
        m = re.findall(r"DOCKERFILE: (\S+)", text)
        self.image = m[-1] if m else None
        run = re.search(r"#\d+ \[[^\]]*\] RUN (.+)", text)
        self.run_cg = None
        if run:
            key = " ".join(run.group(1).split())
            for pid, (comm, cg, c) in self.cmd.items():
                if cg not in ("-", "..") and (" ".join(c.split()) == key or (comm in ("sh", "bash", "dash") and key in " ".join(c.split()))):
                    self.run_cg = cg
                    break
        self.our_cg = set()
        mine = [c for _, _, c, me in self.podcmd.values() if me]  # this process's own command line, as it saw itself
        for pid, (comm, cg, c) in self.cmd.items():
            if comm == "imgbuild" and ((mine and c.strip() == mine[0].strip()) or (not mine and self.image and self.image in c)):
                self.our_cg.add(pid)
        self.our_pids = set(self.our_cg)
        self.our_cgs = {self.cmd[p][1] for p in self.our_pids}
        self.cg_comms = collections.defaultdict(collections.Counter)
        for pid, (comm, cg, c) in self.cmd.items():
            self.cg_comms[cg][comm] += 1

    def cglabel(self, path):
        """Name for a leaf cgroup path from the `g` lines: this job's RUN container / pod, else the container's main commands."""
        last = path.rsplit("/", 1)[-1]
        if last == self.run_cg:
            return "THIS JOB'S RUN CONTAINER"
        if last in self.our_cgs:
            return "THIS JOB'S imgbuild pod"
        if last in self.cg_comms:
            top = [c for c, _ in self.cg_comms[last].most_common(3)]
            return f"container {last[:6]} ({'/'.join(top)})"
        return path

    def role(self, pid, comm, cg):
        if self.run_cg and cg == self.run_cg:
            return f"RUN container: {comm}"
        if pid in self.our_pids:
            return "imgbuild (this job)"
        if comm in DOCKER:
            return "docker/containerd"
        if comm in ("zstd", "tar", "gzip"):
            return f"{comm} (helper/other)"
        if comm.startswith(KERNEL):
            return "kernel io/housekeeping"
        if comm == "imgbuild":
            return "imgbuild (other job)"
        return f"other: {comm}"


def window_stats(d, lo, hi):
    out = {}
    sel = [p for p in d.procs if lo <= p[0] < hi]
    cpu, rd, wr = collections.defaultdict(float), collections.defaultdict(float), collections.defaultdict(float)
    for t, pid, comm, cg, c, r, w, st in sel:
        k = d.role(pid, comm, cg)
        cpu[k] += c * d.dt
        rd[k] += r * d.dt
        wr[k] += w * d.dt
    out["cpu"], out["rd"], out["wr"] = cpu, rd, wr
    n = [v for t, v in d.node if lo <= t < hi]
    avg = lambda i: sum(x[i] for x in n) / len(n) if n else float("nan")
    out["node"] = dict(cpu=avg(0), iowait=avg(1), cpupsi=avg(2), iopsi=avg(3), iofull=avg(4), busy=avg(5), wr=avg(6), rd=avg(7), rx=avg(8),
                       running=avg(10), blocked=avg(11), rd_await=avg(14), wr_await=avg(15), inflight=avg(16), dirty=avg(17), wb=avg(18),
                       wr_iops=avg(13), n=len(n))
    w = collections.Counter()
    for t, pid, comm, cg, wc in d.dstate:
        if lo <= t < hi:
            w[(d.role(pid, comm, cg) if (d.run_cg and cg == d.run_cg) else "other", wc)] += d.dt
    out["dwait"] = w
    return out


def bucket(path):
    """Where a written file lives, coarsely: /app/.venv/lib/python3.12/site-packages/x/y -> /app/.venv/lib/python3.12/site-packages."""
    m = re.match(r"(.*?/site-packages)/", path)
    if m:
        return m.group(1)
    parts = path.split("/")
    return "/".join(parts[:5]) if len(parts) > 5 else "/".join(parts[:-1]) or path


def extra_lines(d, lo, hi):
    L = []
    sel = lambda rows: [r for r in rows if lo <= r[0] < hi]
    o = [v for t, v in sel(d.own)]
    if o:
        av = lambda i: sum(x[i] for x in o) / len(o)
        mx = lambda i: max(x[i] for x in o)
        L.append(f"    THIS imgbuild PROCESS: cpu {av(0) / 100:.2f} user + {av(1) / 100:.2f} sys cores, rss {mx(2):.0f} MB peak, {mx(3):.0f} threads | syscalls read() {av(4):.0f} MB/s write() {av(5):.0f} MB/s, storage read {av(6):.0f} MB/s write {av(7):.0f} MB/s | "
                 f"network (pod) rx {av(8):.0f} MB/s (peak {mx(8):.0f}), tx {av(9):.0f} MB/s (peak {mx(9):.0f})")
        L.append(f"    THIS POD (cgroup): {av(10):.2f} cores used, CPU-throttled {av(11):.0f}% of the time | memory {mx(12):.0f} MB ({mx(13):.0f} MB page cache, dirty+writeback {mx(14):.0f} MB) | "
                 f"storage rd {av(15):.0f} MB/s wr {av(16):.0f} MB/s | stalled on cpu {av(17):.0f}% io {av(18):.0f}% memory {av(19):.0f}%")
    cpu, wr, rd = collections.defaultdict(float), collections.defaultdict(float), collections.defaultdict(float)
    dw = collections.Counter()
    for t, pid, comm, ppid, c, r, w, st, wc in sel(d.pod):
        cpu[comm] += c * 0.4
        wr[comm] += w * 0.4
        rd[comm] += r * 0.4
        if st == "D":
            dw[(comm, wc)] += 0.4
    for label, dd, unit, thr in (("CPU", cpu, "core-s", 0.2), ("storage write", wr, "MB", 1), ("storage read", rd, "MB", 1)):
        top = [f"{k} {v:.1f}" for k, v in sorted(dd.items(), key=lambda x: -x[1])[:5] if v >= thr]
        if top:
            L.append(f"    this pod's processes, {label} ({unit}): " + "; ".join(top))
    if dw:
        L.append("    this pod's processes in D state (comm / kernel function, s): " + "; ".join(f"{k[0]}/{k[1]} {v:.1f}" for k, v in dw.most_common(4)))
    m = [v for t, v in sel(d.mem)]
    if m:
        av = lambda i: sum(x[i] for x in m) / len(m)
        L.append(f"    node memory: {min(x[0] for x in m):.0f} MB available (min), page cache {av(1):.0f} MB, swap used {av(3):.0f} MB | memory stall {av(4):.1f}% (full {av(5):.1f}%) | major faults {av(6):.0f}/s, "
                 f"direct reclaim {av(7):.0f} pages/s, kswapd {av(8):.0f} pages/s | kernel wrote back {av(9):.0f} MB/s, dirtied {av(10):.0f} MB/s | {av(11) / 1000:.0f}k context switches/s, VM steal {av(12):.1f}%")
    dev = []
    for name, rows in d.dev.items():
        v = [x for t, x in rows if lo <= t < hi]
        if v:
            n = max(1, round((hi - lo) / d.dt))
            fl = sum(x[6] for x in v if len(x) > 6) / n
            fm = [x[7] for x in v if len(x) > 7 and x[6] > 0]
            dev.append((name, sum(x[0] for x in v) / n, sum(x[1] for x in v) / n, sum(x[2] for x in v) / n, sum(x[3] for x in v) / len(v), max(x[4] for x in v), fl, sum(fm) / len(fm) if fm else 0))
    if dev:
        L.append("    block devices, avg MB/s written / read, busy %, write await ms, max queue (every layer counts the same bytes again): " +
                 "; ".join(f"{n} {w:.0f}/{r:.0f}, {b:.0f}%, {a:.0f} ms, q{q:.0f}" + (f", {fl:.0f} cache flushes/s at {fm:.1f} ms each" if fl >= 1 else "") for n, w, r, b, a, q, fl, fm in sorted(dev, key=lambda x: -x[1])))
    cg_w, cg_r = collections.defaultdict(float), collections.defaultdict(float)
    for t, w, r_, io, path in sel(d.cgio):
        cg_w[d.cglabel(path)] += w * d.dt
        cg_r[d.cglabel(path)] += r_ * d.dt
    if cg_w:
        node = [v for t, v in d.node if lo <= t < hi]
        nw = sum(x[6] for x in node) * d.dt
        L.append(f"    BLOCK-LAYER WRITES BY CONTAINER (kernel's own count; {sum(cg_w.values()):.0f} MB listed of {nw:.0f} MB the node disk wrote): " +
                 "; ".join(f"{k} {v:.0f}" for k, v in sorted(cg_w.items(), key=lambda x: -x[1])[:7] if v >= 1))
        rd_ = [f"{k} {v:.0f}" for k, v in sorted(cg_r.items(), key=lambda x: -x[1])[:5] if v >= 5]
        if rd_:
            L.append("    block-layer reads by container (MB): " + "; ".join(rd_))
    canc = collections.defaultdict(float)
    for t, pid, comm, cg, mbps in sel(d.cancel):
        canc[d.role(pid, comm, cg)] += mbps * d.dt
    top_c = [f"{k} {v:.0f}" for k, v in sorted(canc.items(), key=lambda x: -x[1])[:5] if v >= 5]
    if top_c:
        L.append("    written then deleted before reaching the disk (MB, process counters): " + "; ".join(top_c))
    est, seq = collections.defaultdict(float), collections.defaultdict(float)
    last = {}
    for t, pid, comm, cg, mbps, files in sel(d.writes):
        role = "RUN container" if d.run_cg and cg == d.run_cg else ("imgbuild (this job)" if pid in d.our_pids else comm)
        for path, pos in files:
            est[(role, bucket(path))] += mbps * d.dt / len(files)
            if (pid, path) in last and pos >= last[(pid, path)]:
                seq[(role, path)] += pos - last[(pid, path)]
            last[(pid, path)] = pos
    if est:
        L.append("    where writes go (top-2 writers' open files per sample, MB estimated): " + "; ".join(f"{r}: {b} {v:.0f}" for (r, b), v in sorted(est.items(), key=lambda x: -x[1])[:6] if v >= 1))
    big = [f"{r}: {p} {v:.0f}" for (r, p), v in sorted(seq.items(), key=lambda x: -x[1])[:5] if v >= 5]
    if big:
        L.append("    big files grown while open (MB): " + "; ".join(big))
    return L


def table(d, wins):
    L = []
    for name, lo, hi in wins:
        if hi - lo < 0.3:
            continue
        s = window_stats(d, lo, hi)
        n = s["node"]
        L.append(f"=== {name}   {lo:.1f}-{hi:.1f}s ({hi - lo:.1f} s)")
        if not n["n"]:
            L.append("    no samples in this window")
            continue
        L.append(f"    node: cpu {n['cpu']:.0f}% busy, iowait {n['iowait']:.0f}%, runnable {n['running']:.0f}, blocked-on-io {n['blocked']:.0f} procs | "
                 f"disk busy {n['busy']:.0f}%, wr {n['wr']:.0f} MB/s ({n['wr_iops']:.0f} iops), rd {n['rd']:.0f} MB/s | await wr {n['wr_await']:.1f} ms rd {n['rd_await']:.1f} ms, queue {n['inflight']:.0f} | "
                 f"I/O pressure {n['iopsi']:.0f}% (full {n['iofull']:.0f}%), CPU pressure {n['cpupsi']:.0f}% | dirty {n['dirty']:.0f} MB writeback {n['wb']:.0f} MB")
        for label, key, unit in (("CPU", "cpu", "core-s"), ("process write counters (over-count: use the container line)", "wr", "MB"), ("process read counters", "rd", "MB")):
            top = sorted(s[key].items(), key=lambda x: -x[1])[:6]
            top = [f"{k} {v:.1f}" for k, v in top if v >= (0.2 if key == "cpu" else 1)]
            if top:
                L.append(f"    {label:13} ({unit}): " + "; ".join(top))
        L += extra_lines(d, lo, hi)
        mine = {k: v for k, v in s["cpu"].items() if k.startswith("RUN container")}
        if d.run_cg:
            L.append("    THIS JOB'S RUN CONTAINER: " + (", ".join(f"{k.split(': ')[1]} {v:.2f} core-s, wrote {s['wr'].get(k, 0):.0f} MB" for k, v in sorted(mine.items(), key=lambda x: -x[1])) or "no process above 1% CPU or 0.3 MB/s"))
        mine_d = collections.Counter({k[1]: v for k, v in s["dwait"].items() if k[0] != "other"})
        if mine_d:
            L.append("    RUN container stuck in D state (waiting on the kernel), seconds by kernel function: " + "; ".join(f"{k} {v:.1f}" for k, v in mine_d.most_common(4)))
        oth = collections.Counter()
        for k, v in s["dwait"].items():
            if k[0] == "other": oth[k[1]] += v
        if oth:
            L.append("    other processes in D state on the node (capped 8 per sample): " + "; ".join(f"{a} {b:.1f}" for a, b in oth.most_common(4)))
    return "\n".join(L)


def span_table(prof, res, min_dur=0.3):
    L = ["", "=== SPANS (imgbuild's own timings; cpu / network are this process's / pod's while the span ran, overlapping spans share them)"]
    L.append(f"    {'start':>7} {'dur':>6}  {'cat':<14} {'wrote MB':>8} {'cpu s':>6} {'rx MB':>7} {'tx MB':>7}  label")
    for (st, du, mb, c, l), (cpu, rx, tx) in sorted(zip(prof, res), key=lambda x: x[0][0]):
        if du >= min_dur:
            f = lambda v, w, p=1: f"{v:{w}.{p}f}" if v is not None else " " * (w - 1) + "-"
            L.append(f"    {st:7.1f} {du:6.1f}  {c:<14} {mb:8.0f} {f(cpu, 6)} {f(rx, 7, 0)} {f(tx, 7, 0)}  {l[:80]}")
    return "\n".join(L)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--log"); ap.add_argument("--run"); ap.add_argument("--job")
    ap.add_argument("--host", default="github.com"); ap.add_argument("--repo")
    ap.add_argument("-o", "--out", default="timeline.png"); ap.add_argument("--top", type=int, default=10)
    a = ap.parse_args()
    if not a.log and not (a.run and a.job and a.repo):
        ap.error("pass --log or all of --repo, --run and --job")
    text = open(a.log, errors="ignore").read() if a.log else fetch(a.run, a.job, a.host, a.repo)
    prof, res, samp, events, wall = parse(text)
    d = Data(samp, text)
    if not d.cpu:
        sys.exit("no `samp` lines in the log: run with IMGBUILD_PROFILE=1 IMGBUILD_SAMPLE=<image>")
    t_end = max(wall or 0, d.cpu[-1][0]) + 1

    spans = lambda cat: [(s, s + du) for s, du, _, c, _ in prof if c == cat]
    wins = []
    uv = [t for t, e in events]
    for lo, hi in spans("run-load"): wins.append(("docker load layers into daemon", lo, hi))
    for lo, hi in spans("run-create"): wins.append(("docker create container", lo, hi))
    for lo, hi in spans("run-exec"):
        inside = sorted(t for t in uv if lo <= t <= hi)
        if inside:
            wins += [("RUN: container start -> first output line", lo, inside[0]), ("RUN: command producing output", inside[0], inside[-1]),
                     ("RUN: last output line -> container exit", inside[-1], hi)]
        else:
            wins.append(("RUN: whole", lo, hi))
    for lo, hi in spans("run-layer"): wins.append(("read RUN result from overlay upper dir -> layer", lo, hi))
    for lo, hi in spans("export"): wins.append(("export: push image to registry", lo, hi))
    for lo, hi in spans("seed-wait"): wins.append(("seed wait", lo, hi))
    wins.append(("WHOLE JOB", 0, t_end))
    txt = table(d, wins) + "\n" + span_table(prof, res)
    head = f"samples: {len(d.cpu)} every {d.dt:.2f}s; run container cgroup: {d.run_cg}; this job's imgbuild pids: {sorted(d.our_pids) or 'not identified'}\n"
    open(a.out.rsplit(".", 1)[0] + ".txt", "w").write(head + txt + "\n")
    print(head + txt)

    extra_panels = [k for k, have in (("own", d.own), ("cg", d.cgio), ("dev", d.dev), ("mem", d.mem)) if have]
    ratios = [2.4, 2, 2.4, 2.2, 1.6, 1.8, 1.8] + [1.8] * len(extra_panels)
    fig, ax = plt.subplots(len(ratios), 1, figsize=(17, 24 + 3.4 * len(extra_panels)), sharex=True, gridspec_kw={"height_ratios": ratios})
    a0 = ax[0]
    for i, (name, col, f) in enumerate(LANES):
        for s, du, mb, c, l in prof:
            if f(c, l) and du > 0.02:
                a0.barh(i, du, left=s, color=col, height=0.7)
    for t, e in events:
        if re.match(r"(Building|Built|Prepared|Installed \d+|Resolved|Using|Downloaded)", e) or re.match(r"\d+\.\d+ (Building|Built|Prepared|Installed|Resolved|Using)", e):
            e2 = re.sub(r"^\d+\.\d+\s+", "", e)
            a0.plot([t, t], [1.6, 2.4], color="black", lw=0.6)
            if not e2.startswith("Downloaded"):
                a0.text(t, 1.45, e2[:30], rotation=35, fontsize=6.5, ha="left", va="top")
    a0.set_yticks(range(len(LANES))); a0.set_yticklabels([l[0] for l in LANES], fontsize=8); a0.invert_yaxis()
    a0.set_title("imgbuild phases (ticks in the RUN lane: lines the command printed)", fontsize=9)

    ts = np.array([t for t, _ in d.cpu]); nc = max(len(r) for _, r in d.cpu)
    M = np.full((nc, len(d.cpu)), np.nan)
    for j, (_, r) in enumerate(d.cpu): M[:len(r), j] = r
    order = np.argsort(-np.nan_to_num(M).sum(axis=1))
    im = ax[1].pcolormesh(np.append(ts, ts[-1] + d.dt), np.arange(nc + 1), M[order], cmap="inferno", vmin=0, vmax=100)
    ax[1].set_ylabel(f"{nc} cores (busiest on top)"); ax[1].invert_yaxis()
    fig.colorbar(im, ax=ax[1], pad=0.005, label="busy %")

    grid = np.round(ts, 3)
    idx = {round(t, 3): i for i, t in enumerate(grid)}

    def stack(axis, key, label, ylabel):
        by = collections.defaultdict(lambda: np.zeros(len(grid)))
        for t, pid, comm, cg, c, r, w, st in d.procs:
            i = idx.get(round(t, 3))
            if i is None: continue
            by[d.role(pid, comm, cg)][i] += {"cpu": c, "wr": w, "rd": r}[key]
        tot = sorted(by, key=lambda k: -by[k].sum())
        keep = tot[: a.top]
        rest = sum((by[k] for k in tot[a.top:]), np.zeros(len(grid)))
        cols = plt.cm.tab20(np.linspace(0, 1, len(keep) + 1))
        for j, k in enumerate(keep):  # the RUN container always gets red
            if k.startswith("RUN container"): cols[j] = (0.84, 0.15, 0.16, 1)
        axis.stackplot(ts, *[by[k] for k in keep], rest, labels=keep + ["rest"], colors=cols)
        axis.set_ylabel(ylabel); axis.legend(fontsize=7, loc="upper right", ncol=2)

    stack(ax[2], "cpu", "cpu", "cores of CPU used, by process")
    stack(ax[3], "wr", "wr", "disk writes MB/s, by process")
    nt = np.array([t for t, _ in d.node]); N = np.array([v for _, v in d.node])
    ax[4].plot(nt, N[:, 6], label="node disk write MB/s"); ax[4].plot(nt, N[:, 7], label="node disk read MB/s")
    ax[4].plot(nt, N[:, 8], label="net rx MB/s")
    ax[4].set_ylabel("MB/s"); ax[4].legend(fontsize=7, ncol=3, loc="upper left")
    b4 = ax[4].twinx(); b4.plot(nt, N[:, 15], color="crimson", lw=1, label="write await ms"); b4.plot(nt, N[:, 14], color="orange", lw=1, label="read await ms")
    b4.plot(nt, N[:, 16], color="gray", lw=1, label="requests in flight"); b4.set_ylabel("ms / queue"); b4.legend(fontsize=7, ncol=3, loc="upper right")
    ax[5].plot(nt, N[:, 1], label="iowait % of CPU"); ax[5].plot(nt, N[:, 3], label="I/O pressure some %"); ax[5].plot(nt, N[:, 4], label="I/O pressure full %")
    ax[5].plot(nt, N[:, 2], label="CPU pressure %"); ax[5].plot(nt, N[:, 5], label="disk busy %", color="purple", lw=0.8)
    ax[5].legend(fontsize=7, ncol=5, loc="upper right"); ax[5].set_ylabel("%")
    ax[6].plot(nt, N[:, 11], label="procs blocked on I/O", color="crimson"); ax[6].plot(nt, N[:, 10], label="procs runnable", color="teal")
    ax[6].set_ylabel("processes"); ax[6].legend(fontsize=7, loc="upper left")
    b6 = ax[6].twinx(); b6.plot(nt, N[:, 17], color="brown", label="dirty page cache MB"); b6.plot(nt, N[:, 18], color="olive", label="writeback MB")
    b6.set_ylabel("MB"); b6.legend(fontsize=7, loc="upper right")
    for i, kind in enumerate(extra_panels, start=7):
        if kind == "own":
            ot = np.array([t for t, _ in d.own]); O = np.array([v for _, v in d.own])
            ax[i].plot(ot, (O[:, 0] + O[:, 1]) / 100, label="imgbuild process cpu (cores)", color="black"); ax[i].plot(ot, O[:, 10], label="pod cgroup cpu (cores)", color="tab:blue")
            ax[i].plot(ot, O[:, 11] / 100, label="pod throttled by CPU limit (fraction of time)", color="crimson")
            ax[i].set_ylabel("cores"); ax[i].legend(fontsize=7, loc="upper left", ncol=3)
            b = ax[i].twinx(); b.plot(ot, O[:, 8], label="network rx MB/s (pod)", color="tab:green"); b.plot(ot, O[:, 9], label="network tx MB/s (pod)", color="tab:orange")
            b.plot(ot, O[:, 7], label="imgbuild storage write MB/s", color="tab:purple", lw=0.8); b.plot(ot, O[:, 16], label="pod storage write MB/s", color="tab:brown", lw=0.8)
            b.set_ylabel("MB/s"); b.legend(fontsize=7, loc="upper right", ncol=2)
            ax[i].set_title("this imgbuild process and its pod (own /proc and cgroup): CPU, network, storage", fontsize=9)
        elif kind == "cg":
            by = collections.defaultdict(lambda: np.zeros(len(grid)))
            for t, w, r_, io, path in d.cgio:
                j = idx.get(round(t, 3))
                if j is not None: by[d.cglabel(path)][j] += w
            tot = sorted(by, key=lambda k: -by[k].sum())
            keep = tot[:9]
            rest = sum((by[k] for k in tot[9:]), np.zeros(len(grid)))
            cols = list(plt.cm.tab20(np.linspace(0, 1, len(keep) + 1)))
            for j, k in enumerate(keep):
                if k.startswith("THIS JOB'S RUN"): cols[j] = (0.84, 0.15, 0.16, 1)
                elif k.startswith("THIS JOB'S"): cols[j] = (0.1, 0.1, 0.1, 1)
            ax[i].stackplot(ts, *[by[k] for k in keep], rest, labels=keep + ["rest"], colors=cols)
            ax[i].set_ylabel("MB/s written to disk"); ax[i].legend(fontsize=7, loc="upper right", ncol=2)
            ax[i].set_title("block-layer writes per container (kernel io.stat of each cgroup: bytes that reach the device, unlike process counters)", fontsize=9)
        elif kind == "dev":
            for name, rows in sorted(d.dev.items(), key=lambda kv: -sum(x[1][0] for x in kv[1])):
                ax[i].plot([t for t, _ in rows], [x[0] for _, x in rows], label=f"{name} write MB/s", lw=1)
            ax[i].set_ylabel("MB/s written"); ax[i].legend(fontsize=7, loc="upper left", ncol=4)
            ax[i].set_title("every block-device layer (dm = encryption, md = RAID, nvme = disk): the same bytes counted at each layer", fontsize=9)
        else:
            mt = np.array([t for t, _ in d.mem]); Mm = np.array([v for _, v in d.mem])
            ax[i].plot(mt, Mm[:, 0], label="MemAvailable MB"); ax[i].plot(mt, Mm[:, 1], label="page cache MB"); ax[i].plot(mt, Mm[:, 2], label="anonymous MB")
            ax[i].set_ylabel("MB"); ax[i].legend(fontsize=7, loc="upper left", ncol=3)
            b = ax[i].twinx(); b.plot(mt, Mm[:, 7] / 1000, label="direct reclaim kpages/s", color="crimson"); b.plot(mt, Mm[:, 4], label="memory stall %", color="black", lw=0.8)
            b.plot(mt, Mm[:, 12], label="VM steal %", color="orange", lw=0.8); b.plot(mt, Mm[:, 6] / 100, label="major faults x100/s", color="gray", lw=0.8)
            b.legend(fontsize=7, loc="upper right", ncol=2); ax[i].set_title("node memory pressure, reclaim and VM steal", fontsize=9)
    ax[-1].set_xlabel("seconds since imgbuild started"); ax[-1].set_xlim(0, t_end)
    for x in ax:
        x.grid(alpha=0.25)
        for _, lo, hi in wins[:-1]:
            if hi - lo > 0.3: x.axvline(lo, color="k", lw=0.4, ls=":")
    fig.suptitle("whole node, ~0.4 s samples (other jobs on the node included). Red in the process panels = this job's RUN container", fontsize=9)
    fig.tight_layout(rect=(0, 0, 1, 0.985)); fig.savefig(a.out, dpi=100)
    print(a.out)


if __name__ == "__main__":
    main()
