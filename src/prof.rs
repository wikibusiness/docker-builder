//! IMGBUILD_PROFILE=1: where a build's time and disk writes go. Phases are timed as spans (`span`), and
//! `report` prints them at the end with the node's disk and I/O-stall counters, folded into a group in
//! GitHub Actions so the log still reads like `docker build`. Lines `prof <start> <dur> <written MB> <cpu s> <net rx MB> <net tx MB> <cat> <label>`
//! are for scripts. A sampler thread inside the process (`start_self_sampler`) records its own CPU, memory, I/O and network and its pod's cgroup
//! every 0.4 s (`o`, `q`, `y` lines), and `IMGBUILD_SAMPLE` adds the node view from the helper container (see `SAMPLER`).

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

struct Rec {
    cat: &'static str,
    label: String,
    start: f64,
    dur: f64,
    wrote: u64,
    /// This process's CPU seconds and the pod's network bytes received and sent while the span ran.
    cpu: f64,
    rx: u64,
    tx: u64,
}

static RECS: Mutex<Vec<Rec>> = Mutex::new(Vec::new());

fn t0() -> Instant {
    static T: OnceLock<Instant> = OnceLock::new();
    *T.get_or_init(Instant::now)
}

pub fn on() -> bool {
    static P: OnceLock<bool> = OnceLock::new();
    *P.get_or_init(|| std::env::var_os("IMGBUILD_PROFILE").is_some_and(|v| v != "0"))
}

/// Timed until dropped; its write count is this whole process's, so overlapping spans share bytes.
pub struct Span {
    cat: &'static str,
    pub label: String,
    t: Instant,
    w: u64,
    c: Cnt,
}

pub fn span(cat: &'static str, label: impl FnOnce() -> String) -> Option<Span> {
    on().then(|| {
        t0();
        Span { cat, label: label(), t: Instant::now(), w: self_io()[1], c: cnt() }
    })
}

/// Adds to a span's label what is known only at its end (e.g. whether a blob was uploaded).
pub fn note(s: &mut Option<Span>, what: &str) {
    if let Some(s) = s {
        s.label.push(' ');
        s.label.push_str(what);
    }
}

/// A span that began at `t`.
pub fn record(cat: &'static str, label: impl FnOnce() -> String, t: Instant) {
    if on() {
        RECS.lock().unwrap().push(Rec { cat, label: label(), start: t.saturating_duration_since(t0()).as_secs_f64(), dur: t.elapsed().as_secs_f64(), wrote: 0, cpu: 0.0, rx: 0, tx: 0 });
    }
}

impl Drop for Span {
    fn drop(&mut self) {
        let wrote = self_io()[1].saturating_sub(self.w);
        let c = cnt();
        RECS.lock().unwrap().push(Rec {
            cpu: (c.user + c.sys).saturating_sub(self.c.user + self.c.sys) as f64 / hz(),
            rx: c.rx.saturating_sub(self.c.rx),
            tx: c.tx.saturating_sub(self.c.tx),
            cat: self.cat,
            label: std::mem::take(&mut self.label),
            start: self.t.saturating_duration_since(t0()).as_secs_f64(),
            dur: self.t.elapsed().as_secs_f64(),
            wrote,
        });
    }
}

fn read(p: &str) -> String {
    fs::read_to_string(p).unwrap_or_default()
}

fn hz() -> f64 {
    static H: OnceLock<f64> = OnceLock::new();
    *H.get_or_init(|| (unsafe { libc::sysconf(libc::_SC_CLK_TCK) }).max(1) as f64)
}

/// The value after `key` on the line that starts with `key` ("usage_usec 123", "anon 123").
fn kv(t: &str, key: &str) -> u64 {
    t.lines().find_map(|l| l.strip_prefix(key)?.strip_prefix(' ')?.trim().parse().ok()).unwrap_or(0)
}

/// `name (comm) S ppid ...` -> (comm, fields after the state, state first).
fn stat_fields(t: &str) -> Option<(&str, Vec<&str>)> {
    let (o, c) = (t.find('(')?, t.rfind(')')?);
    Some((&t[o + 1..c], t.get(c + 2..)?.split_whitespace().collect()))
}

/// Cumulative counters of this process, its network namespace and its cgroup, read at one moment. Missing files count as 0.
#[derive(Clone, Default)]
struct Cnt {
    /// This process, in clock ticks.
    user: u64,
    sys: u64,
    rchar: u64,
    wchar: u64,
    rd: u64,
    wr: u64,
    rss: u64,
    threads: u64,
    /// Network bytes of this namespace, without `lo`.
    rx: u64,
    tx: u64,
    /// This container's cgroup: CPU microseconds, throttled microseconds, memory bytes now / of page cache / dirty, storage bytes read / written, stall microseconds.
    cg_cpu: u64,
    cg_thr: u64,
    cg_mem: u64,
    cg_file: u64,
    cg_dirty: u64,
    cg_rd: u64,
    cg_wr: u64,
    psi: [u64; 3],
}

fn cnt() -> Cnt {
    let mut c = Cnt::default();
    if let Some((_, f)) = stat_fields(&read("/proc/self/stat")) {
        let n = |i: usize| f.get(i).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
        (c.user, c.sys, c.threads, c.rss) = (n(11), n(12), n(17), n(21) * 4096);
    }
    let io = read("/proc/self/io");
    let f = |k: &str| io.lines().find_map(|l| l.strip_prefix(k)).and_then(|v| v.trim().parse().ok()).unwrap_or(0);
    (c.rchar, c.wchar, c.rd, c.wr) = (f("rchar:"), f("wchar:"), f("read_bytes:"), f("write_bytes:"));
    for l in read("/proc/self/net/dev").lines().skip(2) {
        if let Some((n, r)) = l.split_once(':') {
            let v: Vec<u64> = r.split_whitespace().filter_map(|x| x.parse().ok()).collect();
            if n.trim() != "lo" && v.len() > 8 {
                c.rx += v[0];
                c.tx += v[8];
            }
        }
    }
    let (cpu, ms) = (read("/sys/fs/cgroup/cpu.stat"), read("/sys/fs/cgroup/memory.stat"));
    (c.cg_cpu, c.cg_thr) = (kv(&cpu, "usage_usec"), kv(&cpu, "throttled_usec"));
    (c.cg_mem, c.cg_file, c.cg_dirty) = (read("/sys/fs/cgroup/memory.current").trim().parse().unwrap_or(0), kv(&ms, "file"), kv(&ms, "file_dirty") + kv(&ms, "file_writeback"));
    for l in read("/sys/fs/cgroup/io.stat").lines() {
        for w in l.split_whitespace() {
            if let Some(v) = w.strip_prefix("rbytes=") {
                c.cg_rd += v.parse().unwrap_or(0);
            } else if let Some(v) = w.strip_prefix("wbytes=") {
                c.cg_wr += v.parse().unwrap_or(0);
            }
        }
    }
    c.psi = [psi("/sys/fs/cgroup/cpu.pressure")[0], psi("/sys/fs/cgroup/io.pressure")[0], psi("/sys/fs/cgroup/memory.pressure")[0]];
    c
}

static STOP: AtomicBool = AtomicBool::new(false);

fn push_sample(l: String) {
    let mut s = SAMPLES.lock().unwrap();
    if s.len() < 100_000 {
        s.push((t0().elapsed().as_secs_f64(), l));
    }
}

/// A thread that every 0.4 s records, as lines in `SAMPLES`:
/// `o <user% sys% of one cpu, this process> <rss MB> <threads> <rchar MB/s> <wchar MB/s> <storage rd MB/s> <storage wr MB/s> <net rx MB/s> <net tx MB/s>`
/// then, for the pod's cgroup: `< cores used, throttled %, mem MB, page cache MB, dirty+writeback MB, io rd MB/s, io wr MB/s, cpu/io/memory stall %>`;
/// `q <pid> <comm> <ppid> <user%> <sys%> <rd MB/s> <wr MB/s> <state> <wchan>` for each process in this pid namespace above 1% cpu, 0.3 MB/s or in D state;
/// `y <pid> <ppid> <comm> <cmdline>` once per such process. Opt-in with the node sampler (`IMGBUILD_SAMPLE`), but needs no helper container, and it finds out which
/// process is this one: its `y` line ends with `(this process)`.
fn start_self_sampler() {
    static ONCE: OnceLock<()> = OnceLock::new();
    if !on() || !sample_wanted() || ONCE.set(()).is_err() {
        return;
    }
    spawn_self_sampler();
}

fn spawn_self_sampler() {
    t0();
    std::thread::spawn(|| {
        use std::collections::{HashMap, HashSet};
        let me = std::process::id();
        let (mut prev, mut pt) = (cnt(), Instant::now());
        let mut procs: HashMap<u32, [u64; 4]> = HashMap::new();
        let mut seen: HashSet<u32> = HashSet::new();
        while !STOP.load(Ordering::Relaxed) {
            std::thread::sleep(std::time::Duration::from_millis(400));
            let (c, now) = (cnt(), Instant::now());
            let dt = now.duration_since(pt).as_secs_f64().max(0.01);
            let (tick, mb) = (hz() * dt, |a: u64, b: u64| a.saturating_sub(b) as f64 / 1e6 / dt);
            let d = |a: u64, b: u64| a.saturating_sub(b) as f64;
            let ps = |i: usize| d(c.psi[i], prev.psi[i]) / 1e4 / dt;
            push_sample(format!(
                "o {:.0} {:.0} {:.0} {} {:.1} {:.1} {:.1} {:.1} {:.1} {:.1} {:.2} {:.0} {:.0} {:.0} {:.0} {:.1} {:.1} {:.0} {:.0} {:.0}",
                100.0 * d(c.user, prev.user) / tick, 100.0 * d(c.sys, prev.sys) / tick, c.rss as f64 / 1e6, c.threads, mb(c.rchar, prev.rchar), mb(c.wchar, prev.wchar), mb(c.rd, prev.rd), mb(c.wr, prev.wr),
                mb(c.rx, prev.rx), mb(c.tx, prev.tx), d(c.cg_cpu, prev.cg_cpu) / 1e6 / dt, 100.0 * d(c.cg_thr, prev.cg_thr) / 1e6 / dt, c.cg_mem as f64 / 1e6, c.cg_file as f64 / 1e6,
                c.cg_dirty as f64 / 1e6, mb(c.cg_rd, prev.cg_rd), mb(c.cg_wr, prev.cg_wr), ps(0), ps(1), ps(2)
            ));
            let mut cur: HashMap<u32, [u64; 4]> = HashMap::new();
            for e in fs::read_dir("/proc").into_iter().flatten().flatten() {
                let Some(pid) = e.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else { continue };
                let t = read(&format!("/proc/{pid}/stat"));
                let Some((comm, f)) = stat_fields(&t) else { continue };
                let n = |i: usize| f.get(i).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
                let io = read(&format!("/proc/{pid}/io"));
                let g = |k: &str| io.lines().find_map(|l| l.strip_prefix(k)).and_then(|v| v.trim().parse().ok()).unwrap_or(0);
                let v = [n(11), n(12), g("read_bytes:"), g("write_bytes:")];
                cur.insert(pid, v);
                let Some(o) = procs.get(&pid) else { continue };
                let (u, s) = (100.0 * d(v[0], o[0]) / tick, 100.0 * d(v[1], o[1]) / tick);
                let (r, w) = (mb(v[2], o[2]), mb(v[3], o[3]));
                let state = f.first().copied().unwrap_or("?");
                if u + s >= 1.0 || r >= 0.3 || w >= 0.3 || state == "D" {
                    let name = comm.replace(' ', "_");
                    if seen.insert(pid) {
                        let cl = read(&format!("/proc/{pid}/cmdline")).replace('\0', " ");
                        push_sample(format!("y {pid} {} {name} {}{}", f.get(1).copied().unwrap_or("0"), cl.trim_end().chars().take(400).collect::<String>(), if pid == me { " (this process)" } else { "" }));
                    }
                    let wchan = if state == "D" { read(&format!("/proc/{pid}/wchan")) } else { String::new() };
                    push_sample(format!("q {pid} {name} {} {u:.0} {s:.0} {r:.1} {w:.1} {state} {}", f.get(1).copied().unwrap_or("0"), wchan.trim()));
                }
            }
            procs = cur;
            prev = c;
            pt = now;
        }
    });
}

/// This process's bytes read from and written to storage, and written then deleted before reaching it.
fn self_io() -> [u64; 3] {
    let t = fs::read_to_string("/proc/self/io").unwrap_or_default();
    let f = |k: &str| t.lines().find_map(|l| l.strip_prefix(k)).and_then(|v| v.trim().parse().ok()).unwrap_or(0);
    [f("read_bytes:"), f("write_bytes:"), f("cancelled_write_bytes:")]
}

/// Node-wide and own-cgroup I/O counters at one moment.
pub struct Snap {
    dir: std::path::PathBuf,
    t: Instant,
    io: [u64; 3],
    /// The disk holding `dir`: name, sectors read, sectors written, ms busy (/proc/diskstats).
    disk: Option<(String, u64, u64, u64)>,
    /// Microseconds some / all tasks stalled on I/O, node-wide (/proc/pressure/io).
    node_io: [u64; 2],
    /// The same for this container's cgroup, for I/O and for CPU.
    cg_io: [u64; 2],
    cg_cpu: [u64; 2],
    c: Cnt,
}

fn psi(path: &str) -> [u64; 2] {
    let t = fs::read_to_string(path).unwrap_or_default();
    let total = |kind: &str| t.lines().find(|l| l.starts_with(kind)).and_then(|l| l.rsplit("total=").next()?.trim().parse().ok()).unwrap_or(0);
    [total("some"), total("full")]
}

pub fn snap(dir: &Path) -> Snap {
    start_self_sampler();
    use std::os::unix::fs::MetadataExt;
    let disk = fs::metadata(dir).ok().and_then(|m| {
        #[allow(clippy::unnecessary_cast)]
        let (maj, min) = (libc::major(m.dev() as _) as u64, libc::minor(m.dev() as _) as u64);
        fs::read_to_string("/proc/diskstats").ok()?.lines().find_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            let n = |i: usize| f.get(i).and_then(|v| v.parse::<u64>().ok());
            if n(0)? == maj && n(1)? == min { Some((f[2].to_string(), n(5)?, n(9)?, n(12)?)) } else { None }
        })
    });
    Snap { dir: dir.to_path_buf(), t: Instant::now(), io: self_io(), disk, node_io: psi("/proc/pressure/io"), cg_io: psi("/sys/fs/cgroup/io.pressure"), cg_cpu: psi("/sys/fs/cgroup/cpu.pressure"), c: cnt() }
}

/// Prints one line per 0.5 s tick, from the helper's host-wide /proc (it runs with `--pid=host`):
/// `c <busy% of each cpu>`, `n <busy% iowait% cpu-psi% io-psi-some% io-psi-full% disk-busy% disk-write-MB/s disk-read-MB/s net-rx-MB/s net-tx-MB/s>`
/// then `running blocked rd-iops wr-iops rd-await-ms wr-await-ms inflight dirty-MB writeback-MB`; for every process above 1% of one cpu or 0.3 MB/s of disk I/O
/// `p <pid> <comm> <cgroup> <user%> <sys%> <rd MB/s> <wr MB/s> <state>` (100 = one full cpu), `d` lines for processes stuck in D state with their kernel wait
/// function, and `x` once per new process with its command line.
const SAMPLER: &str = r#"
function lp(s,  p,q){p=0;while((q=index(substr(s,p+1),")"))>0)p+=q;return p}
function cmdline(pid,  c,l){c="tr '\\0' ' ' </proc/" pid "/cmdline 2>/dev/null"; l=""; c | getline l; close(c); return substr(l,1,400)}
function wchan(pid,  f,l){f="/proc/" pid "/wchan"; l=""; getline l < f; close(f); return l}
function gname(p,  n,a,i,s,c){ n=split(p,a,"/"); s=""; for(i=1;i<=n;i++){ c=a[i]; sub(/\.(scope|slice|service)$/,"",c); sub(/^(docker|cri-containerd|crio)-/,"",c); if(length(c)==64) c=substr(c,1,12); else c=substr(c,1,30); s=s (s==""?"":"/") c } return s }
function wfiles(pid,  c,l,i,fdn,r,k,m,b,bi,v,out,a){
 delete tg; delete fw; delete fp; delete us
 c="cd /proc/" pid " 2>/dev/null && { ls -l fd 2>/dev/null; grep -H -E '^(flags|pos)' fdinfo/* 2>/dev/null; }"
 while((c|getline l)>0){
  if((i=index(l," -> "))>0){ split(l,a," "); tg[a[9]]=substr(l,i+4) }
  else if(l ~ /^fdinfo\//){ sub(/^fdinfo\//,"",l); i=index(l,":"); fdn=substr(l,1,i-1); r=substr(l,i+1); if(r ~ /^flags/){ fw[fdn]=(substr(r,length(r),1)%4!=0) } else if(r ~ /^pos/){ split(r,a," "); fp[fdn]=a[2] } }
 }
 close(c)
 out=""
 for(m=0;m<4;m++){ b=-1; bi=""
  for(k in tg){ if(fw[k] && !(k in us) && tg[k] ~ /^\// && tg[k] !~ /^\/(dev|proc|sys)\//){ v=fp[k]+0; if(v>b){b=v;bi=k} } }
  if(bi=="")break
  us[bi]=1; out=out (out==""?"":" ;; ") substr(tg[bi],1,200) "=" sprintf("%.1f",b/1e6)
 }
 return out
}
BEGIN{
 system("mkdir -p /mnt/hostcg; grep -q ' /mnt/hostcg ' /proc/mounts || nsenter --cgroup=/proc/1/ns/cgroup -- mount -t cgroup2 none /mnt/hostcg 2>/dev/null")
 cmd="cd /proc; grep -H '' stat pressure/cpu pressure/io pressure/memory diskstats meminfo vmstat 1/net/dev [0-9]*/stat [0-9]*/cgroup [0-9]*/io 2>/dev/null"
 for(tick=0;tick<3000;tick++){
  delete cur; delete cus; delete pp; delete cm; delete cg; delete st; delete rb; delete wb; delete cb; rx=0; tx=0; nc=0
  while((cmd|getline l)>0){
   i=index(l,":"); fn=substr(l,1,i-1); r=substr(l,i+1)
   if(fn=="stat"){ if(r ~ /^cpu[0-9]/){ split(r,f," "); c=substr(f[1],4)+0; t=0; for(k=2;k<=9;k++)t+=f[k]; ct[c]=t; ci[c]=f[5]; cw[c]=f[6]; if(c+1>nc)nc=c+1 } else if(r ~ /^procs_running/){split(r,f," ");prun=f[2]} else if(r ~ /^procs_blocked/){split(r,f," ");pblk=f[2]} else if(r ~ /^cpu /){split(r,f," ");stl=f[9];ttl=0;for(k=2;k<=9;k++)ttl+=f[k]} else if(r ~ /^ctxt/){split(r,f," ");ctx=f[2]} }
   else if(fn=="meminfo"){ split(r,f," "); if(r ~ /^Dirty:/)dirty=f[2]/1024; else if(r ~ /^Writeback:/)wback=f[2]/1024; else if(r ~ /^MemAvailable:/)avail=f[2]; else if(r ~ /^Cached:/)cached=f[2]; else if(r ~ /^AnonPages:/)anon=f[2]; else if(r ~ /^SwapTotal:/)swt=f[2]; else if(r ~ /^SwapFree:/)swf=f[2] }
   else if(fn=="pressure/memory"){ match(r,/total=[0-9]+/); v=substr(r,RSTART+6,RLENGTH-6)+0; if(r ~ /^some/)pmems=v; else pmemf=v }
   else if(fn=="vmstat"){ split(r,f," "); if(f[1]=="pgmajfault")pgmaj=f[2]; else if(f[1]=="pgscan_direct")pgsd=f[2]; else if(f[1]=="pgscan_kswapd")pgsk=f[2]; else if(f[1]=="nr_written")nrw=f[2]; else if(f[1]=="nr_dirtied")nrd=f[2] }
   else if(fn=="pressure/cpu"){ if(r ~ /^some/){ match(r,/total=[0-9]+/); pcpu=substr(r,RSTART+6,RLENGTH-6)+0 } }
   else if(fn=="pressure/io"){ match(r,/total=[0-9]+/); v=substr(r,RSTART+6,RLENGTH-6)+0; if(r ~ /^some/)pios=v; else pior=v }
   else if(fn=="diskstats"){ split(r,f," "); dn[f[3]]=1; dwr[f[3]]=f[10]; drd[f[3]]=f[6]; dbs[f[3]]=f[13]; dwms[f[3]]=f[11]; dwio[f[3]]=f[8]; dinf2[f[3]]=f[12]; dfl[f[3]]=f[19]+0; dfm[f[3]]=f[20]+0; if(f[3]==disk){dmm=f[1] ":" f[2]; dr=f[6];dw=f[10];db=f[13];nr=f[4];nw=f[8];tr_=f[7];tw_=f[11];inf=f[12]} }
   else if(fn=="1/net/dev"){ ii=index(r,":"); if(ii>0){ nm=substr(r,1,ii-1); gsub(/ /,"",nm); if(nm !~ /^(lo|veth|cali|flannel|cni|docker|br-)/){ split(substr(r,ii+1),f," "); rx+=f[1]; tx+=f[9] } } }
   else if(fn ~ /\/stat$/){ pid=fn; sub(/\/stat$/,"",pid); o=index(r,"("); p=lp(r); cm[pid]=substr(r,o+1,p-o-1); split(substr(r,p+2),f," "); st[pid]=f[1]; pp[pid]=f[2]; cus[pid]=f[12]; cur[pid]=f[12]+f[13] }
   else if(fn ~ /\/cgroup$/){ pid=fn; sub(/\/cgroup$/,"",pid); n=split(r,a,"/"); g=a[n]; sub(/^(docker|cri-containerd)-/,"",g); sub(/\.scope$/,"",g); cg[pid]=substr(g,1,12) }
   else if(fn ~ /\/io$/){ pid=fn; sub(/\/io$/,"",pid); if(r ~ /^read_bytes/){split(r,f," ");rb[pid]=f[2]} else if(r ~ /^write_bytes/){split(r,f," ");wb[pid]=f[2]} else if(r ~ /^cancelled_write_bytes/){split(r,f," ");cb[pid]=f[2]} }
  }
  close(cmd)
  if(dmm!=""){
   gc="find /mnt/hostcg -name io.stat -exec grep -H '^" dmm " ' {} + 2>/dev/null"; delete gw; delete gr; delete gi; delete hc
   while((gc|getline l)>0){ i=index(l,"/io.stat:"); if(i==0)continue; gpath=substr(l,1,i-1); sub(/^\/mnt\/hostcg\/?/,"",gpath); if(gpath=="")continue; gpar=gpath; if(sub(/\/[^\/]*$/,"",gpar)==0)gpar="-"; hc[gpar]=1; split(substr(l,i+9),f," ")
    for(k=2;k<=7;k++){ if(f[k] ~ /^wbytes=/)gw[gpath]=substr(f[k],8)+0; else if(f[k] ~ /^rbytes=/)gr[gpath]=substr(f[k],8)+0; else if(f[k] ~ /^wios=/)gi[gpath]=substr(f[k],6)+0 } }
   close(gc)
  }
  dtj=ct[0]-oct[0]
  if(have && dtj>0){
   s="c"; tb=0; tw=0; td=0
   for(c=0;c<nc;c++){ d=ct[c]-oct[c]; w=cw[c]-ocw[c]; b=d-(ci[c]-oci[c])-w; if(d<=0)d=1; s=s sprintf(" %d",100*b/d); tb+=b; tw+=w; td+=d }
   print s
   dts=dtj/100
   dnr=nr-onr; dnw=nw-onw
   printf "n %d %d %.1f %.1f %.1f %d %.1f %.1f %.1f %.1f %d %d %d %d %.1f %.1f %d %d %d\n", 100*tb/td, 100*tw/td, (pcpu-opcpu)/(dtj*100), (pios-opios)/(dtj*100), (pior-opior)/(dtj*100), (db-odb)/(dts*10), (dw-odw)*512/1e6/dts, (dr-odr)*512/1e6/dts, (rx-orx)/1e6/dts, (tx-otx)/1e6/dts, prun, pblk, dnr/dts, dnw/dts, (dnr>0)?(tr_-otr)/dnr:0, (dnw>0)?(tw_-otw)/dnw:0, inf, dirty, wback
   printf "m %d %d %d %d %.1f %.1f %d %d %d %.1f %.1f %d %.1f\n", avail/1024, cached/1024, anon/1024, (swt-swf)/1024, (pmems-opmems)/(dtj*100), (pmemf-opmemf)/(dtj*100), (pgmaj-opgmaj)/dts, (pgsd-opgsd)/dts, (pgsk-opgsk)/dts, (nrw-onrw)*4096/1e6/dts, (nrd-onrd)*4096/1e6/dts, (ctx-octx)/dts, (ttl>ottl)?100*(stl-ostl)/(ttl-ottl):0
   for(dv in dn){ if(dv ~ /^(md[0-9]+|dm-[0-9]+|nvme[0-9]+n[0-9]+|sd[a-z]+|vd[a-z]+|xvd[a-z]+|mmcblk[0-9]+)$/ && (dv in odwr)){
     kw=(dwr[dv]-odwr[dv])*512/1e6/dts; kr=(drd[dv]-odrd[dv])*512/1e6/dts; kb=(dbs[dv]-odbs[dv])/(dts*10); kn=dwio[dv]-odwio[dv]; ka=(kn>0)?(dwms[dv]-odwms[dv])/kn:0
     kf=dfl[dv]-odfl[dv]; kfm=(kf>0)?(dfm[dv]-odfm[dv])/kf:0
     if(kw>=0.5 || kr>=0.5 || kb>=5 || kf/dts>=5) printf "k %s %.1f %.1f %d %.1f %d %d %.0f %.1f\n", dv, kw, kr, kb, ka, dinf2[dv], kn/dts, kf/dts, kfm
   } }
   delete gv
   for(j=0;j<16;j++){ gb=-1; gp=""
    for(gpath in gw){ if(!(gpath in gv) && !(gpath in hc) && (gpath in ogw)){ dd=gw[gpath]-ogw[gpath]; dr2=gr[gpath]-ogr[gpath]; if(dr2>dd)dd=dr2; if(dd>gb){gb=dd;gp=gpath} } }
    if(gp=="" || gb<0.5e6*dts)break
    gv[gp]=1; printf "g %.1f %.1f %d %s\n", (gw[gp]-ogw[gp])/1e6/dts, (gr[gp]-ogr[gp])/1e6/dts, (gi[gp]-ogi[gp])/dts, gname(gp) }
   nd=0; nwc=0; delete wc; delete wv
   for(pid in cur){ if(pid in ocur){
     d=cur[pid]-ocur[pid]; cpu=d*100/dtj; drb=(rb[pid]-orb[pid])/1e6/dts; dwb=(wb[pid]-owb[pid])/1e6/dts
     if(cm[pid]=="imgbuild" && !(pid in seen)){seen[pid]=1; printf "x %s %s %s ppid=%s %s\n", pid, cm[pid], (cg[pid]!="")?cg[pid]:"-", pp[pid], cmdline(pid)}
     if(cpu>=1 || drb>=0.3 || dwb>=0.3){
       if(!(pid in seen)){seen[pid]=1; c2=cmdline(pid); gsub(/[ \t]+$/,"",c2); printf "x %s %s %s ppid=%s %s\n", pid, cm[pid], (cg[pid]!="")?cg[pid]:"-", pp[pid], c2}
       u=(cus[pid]-ocus[pid])*100/dtj; c1=cm[pid]; gsub(/[ \t]/,"_",c1)
       printf "p %s %s %s %d %d %.1f %.1f %s %.1f\n", pid, c1, (cg[pid]!="")?cg[pid]:"-", u, cpu-u, drb, dwb, st[pid], (cb[pid]-ocb[pid])/1e6/dts
       if(dwb>=3){ wc[pid]=dwb }
     }
     if(st[pid]=="D" && nd<8){ nd++; c1=cm[pid]; gsub(/[ \t]/,"_",c1); printf "d %s %s %s %s\n", pid, c1, (cg[pid]!="")?cg[pid]:"-", wchan(pid) }
   } }
   for(j=0;j<2;j++){ bw=-1; bp=""; for(pid in wc){ if(wc[pid]>bw && !(pid in wv)){bw=wc[pid];bp=pid} } if(bp=="")break; wv[bp]=1
     c1=cm[bp]; gsub(/[ \t]/,"_",c1); ws=wfiles(bp); if(ws!="") printf "w %s %s %s %.1f %s\n", bp, c1, (cg[bp]!="")?cg[bp]:"-", bw, ws }
   fflush()
  }
  have=1
  delete ocur; delete ocus; delete orb; delete owb; delete ocb; for(k in cur){ocur[k]=cur[k];ocus[k]=cus[k];orb[k]=rb[k];owb[k]=wb[k];ocb[k]=cb[k]}
  for(c=0;c<nc;c++){oct[c]=ct[c];oci[c]=ci[c];ocw[c]=cw[c]}
  for(gpath in gw){ogw[gpath]=gw[gpath];ogr[gpath]=gr[gpath];ogi[gpath]=gi[gpath]}
  opmems=pmems;opmemf=pmemf;opgmaj=pgmaj;opgsd=pgsd;opgsk=pgsk;onrw=nrw;onrd=nrd;octx=ctx;ostl=stl;ottl=ttl
  for(dv in dn){odwr[dv]=dwr[dv];odrd[dv]=drd[dv];odbs[dv]=dbs[dv];odwms[dv]=dwms[dv];odwio[dv]=dwio[dv];odfl[dv]=dfl[dv];odfm[dv]=dfm[dv]}
  opcpu=pcpu;opios=pios;opior=pior;odb=db;odw=dw;odr=dr;orx=rx;otx=tx;onr=nr;onw=nw;otr=tr_;otw=tw_
  system("sleep 0.3")
 }
}
"#;

static SAMPLES: Mutex<Vec<(f64, String)>> = Mutex::new(Vec::new());
static SAMPLER_STOP: Mutex<Option<(std::process::Child, std::process::Command)>> = Mutex::new(None);

/// IMGBUILD_SAMPLE=all (or 1), or a comma list matched against this process's arguments (the image name):
/// a sampler per job reads every process on the node every 0.5 s, so it is opt-in.
fn sample_wanted() -> bool {
    let v = std::env::var("IMGBUILD_SAMPLE").unwrap_or_default();
    let args: Vec<String> = std::env::args().collect();
    v.split(',').map(str::trim).filter(|p| !p.is_empty()).any(|p| p == "1" || p == "all" || args.iter().any(|a| a.contains(p)))
}

/// Starts the sampler in the helper container (once). `docker` builds a docker command for the right daemon.
/// Samples are stamped with the time they arrive here, on the same clock as the spans; each covers the 0.5 s before.
pub fn start_sampler(docker: &dyn Fn() -> std::process::Command, helper: &str) {
    use std::io::BufRead;
    use std::process::Stdio;
    static ONCE: OnceLock<()> = OnceLock::new();
    if !on() || !sample_wanted() || ONCE.set(()).is_err() {
        return;
    }
    t0();
    let disk = std::env::var_os("IMGBUILD_CACHE_DIR").map(std::path::PathBuf::from).unwrap_or_else(std::env::temp_dir);
    let disk = snap(&disk).disk.map(|d| d.0).unwrap_or_default();
    // Runner pods have their own PID namespaces, so pids collide between jobs sharing the helper: add a random part.
    let id = format!("{}{:x}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos()));
    let tag = format!("imgbuild-sampler-{id}x");
    let mut c = docker();
    c.args(["exec", helper, "awk", "-v", &format!("tag={tag}"), "-v", &format!("disk={disk}"), SAMPLER]);
    let Ok(mut child) = c.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn() else { return };
    let Some(out) = child.stdout.take() else { return };
    std::thread::spawn(move || {
        for l in std::io::BufReader::new(out).lines().map_while(Result::ok) {
            let t = t0().elapsed().as_secs_f64();
            let mut s = SAMPLES.lock().unwrap();
            if s.len() < 60_000 {
                s.push((t, l));
            }
        }
    });
    // Killing `docker exec` leaves the awk running in the helper; find it by its -v tag. The $t-<pid> pieces are joined only by the shell, so this script's own command line does not match.
    let mut stop = docker();
    stop.args(["exec", helper, "sh", "-c", &format!("t=imgbuild-sampler; for p in /proc/[0-9]*; do grep -qa \"$t-{id}x\" $p/cmdline 2>/dev/null && kill ${{p#/proc/}}; done")]);
    *SAMPLER_STOP.lock().unwrap() = Some((child, stop));
}

fn stop_sampler() {
    if let Some((mut child, mut stop)) = SAMPLER_STOP.lock().unwrap().take() {
        let _ = stop.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status();
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// Total time covered by the intervals, overlaps counted once.
fn covered(mut v: Vec<(f64, f64)>) -> f64 {
    v.sort_by(|a, b| a.0.total_cmp(&b.0));
    let (mut sum, mut end) = (0.0, f64::MIN);
    for (s, e) in v {
        if e > end {
            sum += e - s.max(end);
            end = e;
        }
    }
    sum
}

pub fn report(start: &Snap) {
    if !on() {
        return;
    }
    STOP.store(true, Ordering::Relaxed);
    stop_sampler();
    let end = snap(&start.dir);
    let wall = start.t.elapsed().as_secs_f64();
    let gha = std::env::var_os("GITHUB_ACTIONS").is_some();
    let mb = |b: u64| b as f64 / 1e6;
    let pct = |us: u64| us as f64 / 1e4 / wall;
    let mut out = String::new();
    let mut line = |s: String| {
        out.push_str(&s);
        out.push('\n');
    };
    if gha {
        line("::group::imgbuild profile".into());
    }
    line(format!("profile: wall {wall:.1}s; this process read {:.0} MB, wrote {:.0} MB ({:.0} MB of it deleted before reaching the disk)",
        mb(end.io[0] - start.io[0]), mb(end.io[1] - start.io[1]), mb(end.io[2] - start.io[2])));
    if let (Some(a), Some(b)) = (&start.disk, &end.disk) {
        line(format!("profile: node disk {} (all processes): wrote {:.0} MB, read {:.0} MB, busy {:.0}%", b.0, mb((b.2 - a.2) * 512), mb((b.1 - a.1) * 512),
            (b.3 - a.3) as f64 / 10.0 / wall));
    }
    line(format!("profile: stalled on I/O: node some {:.0}% full {:.0}%; this container I/O {:.0}%, CPU {:.0}%",
        pct(end.node_io[0] - start.node_io[0]), pct(end.node_io[1] - start.node_io[1]), pct(end.cg_io[0] - start.cg_io[0]), pct(end.cg_cpu[0] - start.cg_cpu[0])));
    let (a, b) = (&start.c, &end.c);
    let d = |x: u64, y: u64| x.saturating_sub(y);
    let peak = |p: &str, k: &str| read(p).lines().find_map(|l| l.strip_prefix(k)?.split_whitespace().next()?.parse::<u64>().ok()).unwrap_or(0);
    line(format!("profile: this process: cpu {:.1} s user + {:.1} s sys ({:.1} cores on average), peak memory {:.0} MB, {} threads; network received {:.0} MB, sent {:.0} MB (this network namespace, {:.1} / {:.1} MB/s on average); {:.0} MB read() / {:.0} MB write() syscalls",
        d(b.user, a.user) as f64 / hz(), d(b.sys, a.sys) as f64 / hz(), d(b.user + b.sys, a.user + a.sys) as f64 / hz() / wall, peak("/proc/self/status", "VmHWM:") as f64 / 1e3, b.threads,
        mb(d(b.rx, a.rx)), mb(d(b.tx, a.tx)), mb(d(b.rx, a.rx)) / wall, mb(d(b.tx, a.tx)) / wall, mb(d(b.rchar, a.rchar)), mb(d(b.wchar, a.wchar))));
    if b.cg_cpu > 0 || b.cg_mem > 0 {
        line(format!("profile: this pod's cgroup: cpu {:.1} s ({:.1} cores on average), throttled by its CPU limit for {:.1} s, memory {:.0} MB now / {:.0} MB peak ({:.0} MB page cache), storage read {:.0} MB, written {:.0} MB, stalled on cpu {:.0}% / io {:.0}% / memory {:.0}%",
            d(b.cg_cpu, a.cg_cpu) as f64 / 1e6, d(b.cg_cpu, a.cg_cpu) as f64 / 1e6 / wall, d(b.cg_thr, a.cg_thr) as f64 / 1e6, b.cg_mem as f64 / 1e6,
            read("/sys/fs/cgroup/memory.peak").trim().parse::<u64>().unwrap_or(0) as f64 / 1e6, b.cg_file as f64 / 1e6, mb(d(b.cg_rd, a.cg_rd)), mb(d(b.cg_wr, a.cg_wr)),
            pct(d(b.psi[0], a.psi[0])), pct(d(b.psi[1], a.psi[1])), pct(d(b.psi[2], a.psi[2]))));
    }
    let mut recs = std::mem::take(&mut *RECS.lock().unwrap());
    recs.sort_by(|a, b| a.start.total_cmp(&b.start));
    let mut cats: BTreeMap<&str, Vec<&Rec>> = BTreeMap::new();
    for r in &recs {
        cats.entry(r.cat).or_default().push(r);
    }
    let mut rows: Vec<_> = cats.into_iter().map(|(k, v)| {
        let cov = covered(v.iter().map(|r| (r.start, r.start + r.dur)).collect());
        (k, v.len(), cov, v.iter().map(|r| r.dur).sum::<f64>(), v.iter().map(|r| r.dur).fold(0.0, f64::max), v.iter().map(|r| r.wrote).sum::<u64>(),
            v.iter().map(|r| r.cpu).sum::<f64>(), v.iter().map(|r| r.rx).sum::<u64>(), v.iter().map(|r| r.tx).sum::<u64>())
    }).collect();
    rows.sort_by(|a, b| b.2.total_cmp(&a.2));
    line(format!("profile: {:<16} {:>4} {:>8} {:>8} {:>7} {:>9} {:>8} {:>8} {:>8}", "category", "n", "wall s", "sum s", "max s", "wrote MB", "cpu s", "rx MB", "tx MB"));
    for (k, n, cov, sum, max, w, cpu, rx, tx) in rows {
        line(format!("profile: {k:<16} {n:>4} {cov:>8.2} {sum:>8.2} {max:>7.2} {:>9.0} {cpu:>8.1} {:>8.0} {:>8.0}", mb(w), mb(rx), mb(tx)));
    }
    for r in recs.iter().take(600) {
        line(format!("prof {:.3} {:.3} {:.1} {:.2} {:.1} {:.1} {} {}", r.start, r.dur, mb(r.wrote), r.cpu, mb(r.rx), mb(r.tx), r.cat, r.label.chars().take(90).collect::<String>()));
    }
    if gha {
        line("::endgroup::".into());
    }
    let samples = std::mem::take(&mut *SAMPLES.lock().unwrap());
    if !samples.is_empty() {
        if gha {
            line(format!("::group::imgbuild samples ({} lines)", samples.len()));
        }
        line("samp <t> c <cpu busy%...> | n <busy% iowait% cpu-psi% io-psi-some% io-psi-full% disk-busy% disk-wr-MB/s disk-rd-MB/s net-rx net-tx running blocked rd-iops wr-iops rd-await-ms wr-await-ms inflight dirty-MB writeback-MB> | m <avail-MB cached-MB anon-MB swap-used-MB mem-psi-some% mem-psi-full% major-faults/s direct-reclaim-pages/s kswapd-pages/s fs-written-MB/s fs-dirtied-MB/s ctx-switches/s steal%> | k <device> <wr MB/s> <rd MB/s> <busy%> <wr-await-ms> <inflight> <wr-iops> | p <pid> <comm> <cgroup> <user%> <sys%> <rd MB/s> <wr MB/s> <state> <MB/s written then deleted> | d <pid> <comm> <cgroup> <wchan> (process in D state) | g <wr MB/s> <rd MB/s> <wr-iops> <cgroup path> (block-layer I/O of one leaf cgroup, from the host's io.stat) | w <pid> <comm> <cgroup> <MB/s written> <open files being written: path=MB so far ;; ...> | x <pid> <comm> <cgroup> ppid=<ppid> <cmdline> | o <this process user% sys% rss-MB threads rchar-MB/s wchar-MB/s rd-MB/s wr-MB/s net-rx net-tx, its pod cgroup: cores throttled% mem-MB cache-MB dirty-MB io-rd io-wr cpu-stall% io-stall% mem-stall%> | q <pid> <comm> <ppid> <user%> <sys%> <rd MB/s> <wr MB/s> <state> <wchan> (processes in this pod) | y <pid> <ppid> <comm> <cmdline>".into());
        for (t, l) in &samples {
            line(format!("samp {t:.2} {l}"));
        }
        if gha {
            line("::endgroup::".into());
        }
    }
    eprint!("{out}");
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "linux")]
    #[test]
    fn sampler_prints_cpu_and_node_lines() {
        use std::io::BufRead;
        use std::process::{Command, Stdio};
        let Ok(mut c) = Command::new("awk").args(["-v", "disk=", super::SAMPLER]).stdout(Stdio::piped()).stderr(Stdio::null()).spawn() else { return };
        let lines: Vec<String> = std::io::BufReader::new(c.stdout.take().unwrap()).lines().take(4).map_while(Result::ok).collect();
        let _ = c.kill();
        let _ = c.wait();
        assert!(lines.iter().any(|l| l.starts_with("c ")), "{lines:?}");
        assert!(lines.iter().any(|l| l.starts_with("n ") && l.split(' ').count() == 20), "{lines:?}");
        assert!(lines.iter().any(|l| l.starts_with("m ") && l.split(' ').count() == 14), "{lines:?}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn self_sampler_sees_this_process() {
        super::spawn_self_sampler();
        let t = std::time::Instant::now();
        while t.elapsed().as_millis() < 1300 {
            std::hint::black_box((0..100_000u64).sum::<u64>());
        }
        super::STOP.store(true, std::sync::atomic::Ordering::Relaxed);
        let s = super::SAMPLES.lock().unwrap();
        let o: Vec<&String> = s.iter().filter(|(_, l)| l.starts_with("o ")).map(|(_, l)| l).collect();
        assert!(!o.is_empty() && o.iter().all(|l| l.split(' ').count() == 21), "{o:?}");
        let busy = o.iter().any(|l| l.split(' ').nth(1).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0) > 30.0);
        assert!(busy, "a busy loop should show up as cpu: {o:?}");
        assert!(s.iter().any(|(_, l)| l.starts_with("y ") && l.ends_with("(this process)")), "{:?}", s.iter().map(|x| &x.1).collect::<Vec<_>>());
    }

    #[test]
    fn covered_counts_overlaps_once() {
        assert_eq!(super::covered(vec![(0.0, 2.0), (1.0, 3.0), (5.0, 6.0), (5.5, 5.7)]), 4.0);
    }
}
