//! Builds a Dockerfile for several platforms at once, without BuildKit.
//!
//! Every instruction gets a content key (its parent's key, the instruction, and the content it reads),
//! computed before anything is built. The state after each RUN and `COPY --from` is kept in the node's cache
//! (IMGBUILD_CACHE_DIR; without one, in a cache repository as an image tagged `c-<key>`), so a build resumes
//! at the last cached expensive step and
//! usually only re-creates the context COPY layers, natively, for every platform. Only a RUN whose key is
//! new runs, in a container of the stage's platform (`docker run`, emulated when that is not this
//! machine's CPU); stages marked `FROM --platform=$BUILDPLATFORM` always run natively.

use crate::dockerfile::{self, Instr, Stage};
use crate::layer::{Hashing, file_digest};
use crate::registry::{Reg, Ref};
use anyhow::{Context, Result, bail, ensure};
use bytes::Bytes;
use futures::future::{BoxFuture, FutureExt, try_join_all};
use futures::stream::{self, StreamExt, TryStreamExt};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use crate::docker_cache;
use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::io::{self, BufRead, BufWriter, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::OnceCell;

pub struct Opts {
    pub dockerfile: PathBuf,
    pub context: PathBuf,
    pub target: Option<String>,
    pub platforms: Vec<String>,
    pub dests: Vec<String>,
    /// `host/repo` keeping the cached states; default `<dest repo>-cache`
    pub cache: Option<String>,
    pub build_args: Vec<String>,
    pub work: PathBuf,
    pub epoch: u64,
    /// Every platform of the image when this run builds only some of them (see `build`)
    pub all_platforms: Vec<String>,
    /// Image paths (e.g. `/build`) written to a host directory before the image is pushed.
    pub export: Vec<(String, PathBuf)>,
}

/// Part of every cache key: bump it when imgbuild changes the bytes it writes for the same inputs.
const FORMAT: &str = "imgbuild-5";

/// A name part no other imgbuild shares: runner pods on one node share its cache directories, and each pod's
/// PIDs start at 1, so a PID alone collides.
pub fn uniq() -> &'static str {
    static U: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    U.get_or_init(|| {
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos();
        let host = fs::read_to_string("/etc/hostname").unwrap_or_default();
        format!("{}-{}", std::process::id(), &h(&[host.trim(), &t.to_string()])[..12])
    })
}

fn h(parts: &[&str]) -> String {
    let mut d = Sha256::new();
    for p in parts {
        d.update(p.as_bytes());
        d.update([0]);
    }
    hex::encode(d.finalize())
}

fn sha(b: &[u8]) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(b)))
}

fn arch_of(platform: &str) -> &str {
    platform.split('/').nth(1).unwrap_or("amd64")
}

fn oci_layer_type(t: &str) -> String {
    match t {
        "application/vnd.docker.image.rootfs.diff.tar.gzip" => "application/vnd.oci.image.layer.v1.tar+gzip",
        "application/vnd.docker.image.rootfs.diff.tar.zstd" => "application/vnd.oci.image.layer.v1.tar+zstd",
        "application/vnd.docker.image.rootfs.diff.tar" => "application/vnd.oci.image.layer.v1.tar",
        t => t,
    }
    .to_string()
}

/// `$X`, `${X}`, `${X:-default}` and `${X:+alt}`, as Dockerfile instructions expand them.
pub fn expand(s: &str, vars: &BTreeMap<String, String>) -> String {
    let c: Vec<char> = s.chars().collect();
    let (mut out, mut i) = (String::new(), 0);
    while i < c.len() {
        let next = c.get(i + 1).copied();
        match (c[i], next) {
            ('\\', Some('$')) => {
                out.push('$');
                i += 2;
            }
            ('$', Some('{')) => {
                let Some(end) = c[i + 2..].iter().position(|&x| x == '}').map(|e| i + 2 + e) else {
                    out.push('$');
                    i += 1;
                    continue;
                };
                let inner: String = c[i + 2..end].iter().collect();
                let (name, op) = match inner.find(':') {
                    Some(p) => (inner[..p].to_string(), Some(inner[p..].to_string())),
                    None => (inner.clone(), None),
                };
                let set = vars.get(&name).filter(|v| !v.is_empty());
                out.push_str(&match op.as_deref() {
                    Some(o) if o.starts_with(":-") => set.cloned().unwrap_or_else(|| expand(&o[2..], vars)),
                    Some(o) if o.starts_with(":+") => set.map(|_| expand(&o[2..], vars)).unwrap_or_default(),
                    _ => vars.get(&name).cloned().unwrap_or_default(),
                });
                i = end + 1;
            }
            ('$', Some(n)) if n.is_ascii_alphabetic() || n == '_' => {
                let mut j = i + 1;
                while j < c.len() && (c[j].is_ascii_alphanumeric() || c[j] == '_') {
                    j += 1;
                }
                let name: String = c[i + 1..j].iter().collect();
                out.push_str(vars.get(&name).map(String::as_str).unwrap_or(""));
                i = j;
            }
            (x, _) => {
                out.push(x);
                i += 1;
            }
        }
    }
    out
}

/// Go's filepath.Match for one path component.
pub fn fnmatch(p: &[u8], s: &[u8]) -> bool {
    match (p.first(), s.first()) {
        (None, None) => true,
        (Some(b'*'), _) => fnmatch(&p[1..], s) || (!s.is_empty() && fnmatch(p, &s[1..])),
        (Some(b'?'), Some(_)) => fnmatch(&p[1..], &s[1..]),
        (Some(b'['), Some(&c)) => {
            let Some(end) = p.iter().skip(2).position(|&x| x == b']').map(|e| e + 2) else { return false };
            let (neg, set) = match p[1] {
                b'!' | b'^' => (true, &p[2..end]),
                _ => (false, &p[1..end]),
            };
            let (mut hit, mut k) = (false, 0);
            while k < set.len() {
                if k + 2 < set.len() && set[k + 1] == b'-' {
                    hit |= set[k] <= c && c <= set[k + 2];
                    k += 3;
                } else {
                    hit |= set[k] == c;
                    k += 1;
                }
            }
            hit != neg && fnmatch(&p[end + 1..], &s[1..])
        }
        (Some(b'\\'), Some(&c)) if p.len() > 1 => p[1] == c && fnmatch(&p[2..], &s[1..]),
        (Some(&a), Some(&b)) => a == b && fnmatch(&p[1..], &s[1..]),
        _ => false,
    }
}

fn is_glob(s: &str) -> bool {
    s.contains(['*', '?', '['])
}

/// .dockerignore: the last matching pattern decides, and a pattern that matches a directory covers its contents.
struct Ignore(Vec<(bool, Vec<String>)>);

impl Ignore {
    fn load(context: &Path, dockerfile: &Path) -> Ignore {
        let text = fs::read_to_string(format!("{}.dockerignore", dockerfile.display()))
            .or_else(|_| fs::read_to_string(context.join(".dockerignore")))
            .unwrap_or_default();
        Ignore(
            text.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .map(|l| {
                    let (neg, l) = l.strip_prefix('!').map(|r| (true, r.trim())).unwrap_or((false, l));
                    (neg, l.split('/').filter(|c| !c.is_empty() && *c != ".").map(String::from).collect::<Vec<_>>())
                })
                .filter(|(_, c)| !c.is_empty())
                .collect(),
        )
    }

    fn excluded(&self, rel: &str) -> bool {
        let comps: Vec<&str> = rel.split('/').filter(|c| !c.is_empty()).collect();
        let mut ex = false;
        for (neg, pat) in &self.0 {
            if (1..=comps.len()).any(|n| match_comps(pat, &comps[..n])) {
                ex = !neg;
            }
        }
        ex
    }

    fn has_exceptions(&self) -> bool {
        self.0.iter().any(|(neg, _)| *neg)
    }
}

fn match_comps(p: &[String], s: &[&str]) -> bool {
    match p.first() {
        None => s.is_empty(),
        Some(x) if x == "**" => (0..=s.len()).any(|i| match_comps(&p[1..], &s[i..])),
        Some(x) => !s.is_empty() && fnmatch(x.as_bytes(), s[0].as_bytes()) && match_comps(&p[1..], &s[1..]),
    }
}

/// `p` resolved inside `root` as if `root` were `/`: symlinks are followed but cannot leave it.
fn resolve(root: &Path, p: &str, follow_last: bool) -> Result<String> {
    let mut out: Vec<String> = vec![];
    let mut todo: Vec<String> = p.split('/').filter(|c| !c.is_empty()).map(String::from).rev().collect();
    let mut hops = 0;
    while let Some(c) = todo.pop() {
        match c.as_str() {
            "." => continue,
            ".." => {
                out.pop();
                continue;
            }
            _ => {}
        }
        let cand = root.join(out.join("/")).join(&c);
        match fs::symlink_metadata(&cand) {
            Ok(m) if m.file_type().is_symlink() && (!todo.is_empty() || follow_last) => {
                hops += 1;
                ensure!(hops < 64, "too many symlinks resolving {p}");
                let t = fs::read_link(&cand)?.to_string_lossy().to_string();
                if t.starts_with('/') {
                    out.clear();
                }
                todo.extend(t.split('/').filter(|c| !c.is_empty()).map(String::from).rev());
            }
            _ => out.push(c),
        }
    }
    Ok(out.join("/"))
}

/// Paths (relative to `root`, resolved) matching `pattern`, a path that may hold globs.
fn glob(root: &Path, files: Option<&Files>, pattern: &str) -> Result<Vec<String>> {
    let mut cands = vec![String::new()];
    let comps: Vec<&str> = pattern.split('/').filter(|c| !c.is_empty() && *c != ".").collect();
    for (i, comp) in comps.iter().enumerate() {
        let last = i + 1 == comps.len();
        let mut next = vec![];
        for c in &cands {
            if is_glob(comp) {
                let mut names: Vec<String> = match files {
                    Some(f) => f.names(c).cloned().collect(),
                    None => fs::read_dir(root.join(c)).into_iter().flatten().filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().to_string()).collect(),
                };
                names.sort();
                for n in names.into_iter().filter(|n| fnmatch(comp.as_bytes(), n.as_bytes())) {
                    next.push(format!("{c}/{n}"));
                }
            } else {
                next.push(format!("{c}/{comp}"));
            }
        }
        cands = next
            .into_iter()
            .map(|c| match files {
                Some(f) => f.resolve(&c, !last),
                None => resolve(root, &c, !last),
            })
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .filter(|c| match files {
                Some(f) => f.at.contains_key(c),
                None => fs::symlink_metadata(root.join(c)).is_ok(),
            })
            .collect();
    }
    Ok(cands)
}

#[derive(Clone, Copy, Default)]
struct Meta {
    uid: u64,
    gid: u64,
    mode: u32,
    mtime: u64,
}

#[derive(Clone)]
enum Kind {
    Dir,
    File(PathBuf),
    Link(PathBuf),
    /// a file's bytes inside an uncompressed tar: its path, offset, length, and whether it is executable
    Slice(Arc<PathBuf>, u64, u64, bool),
}

/// Reads bytes `off..end` of a file, without moving a shared cursor.
struct SliceReader {
    f: Arc<File>,
    off: u64,
    end: u64,
}

impl Read for SliceReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = (buf.len() as u64).min(self.end - self.off) as usize;
        if n == 0 {
            return Ok(0);
        }
        let n = std::os::unix::fs::FileExt::read_at(self.f.as_ref(), &mut buf[..n], self.off)?;
        self.off += n as u64;
        Ok(n)
    }
}

/// An extracted filesystem, in memory: regular files stay in uncompressed tars instead of being written
/// out, as creating thousands of small files is most of an extract's time (2 s for a 5000-file venv on
/// macOS).
#[derive(Clone)]
enum Node {
    Dir,
    File(Arc<PathBuf>, u64, u64, bool),
    Link(PathBuf),
}

#[derive(Default)]
struct Files {
    at: HashMap<String, Node>,
    kids: HashMap<String, std::collections::BTreeSet<String>>,
}

impl Files {
    /// Puts `n` at `rel`, making missing parent directories.
    fn put(&mut self, rel: &str, n: Node) {
        let (dir, name) = rel.rsplit_once('/').unwrap_or(("", rel));
        if !dir.is_empty() && !matches!(self.at.get(dir), Some(Node::Dir)) {
            self.remove_tree(dir);
            self.put(dir, Node::Dir);
        }
        self.kids.entry(dir.to_string()).or_default().insert(name.to_string());
        self.at.insert(rel.to_string(), n);
    }

    fn remove_tree(&mut self, rel: &str) {
        let Some(n) = self.at.remove(rel) else { return };
        if matches!(n, Node::Dir) {
            for k in self.kids.remove(rel).unwrap_or_default() {
                self.remove_tree(&format!("{rel}/{k}"));
            }
        }
        let (dir, name) = rel.rsplit_once('/').unwrap_or(("", rel));
        if let Some(k) = self.kids.get_mut(dir) {
            k.remove(name);
        }
    }

    fn names(&self, dir: &str) -> impl Iterator<Item = &String> {
        self.kids.get(dir).into_iter().flatten()
    }

    /// `p` with symlinks resolved, as `resolve` does on disk.
    fn resolve(&self, p: &str, follow_last: bool) -> Result<String> {
        let mut out: Vec<String> = vec![];
        let mut todo: Vec<String> = p.split('/').filter(|c| !c.is_empty()).map(String::from).rev().collect();
        let mut hops = 0;
        while let Some(c) = todo.pop() {
            match c.as_str() {
                "." => continue,
                ".." => {
                    out.pop();
                    continue;
                }
                _ => {}
            }
            out.push(c);
            if let Some(Node::Link(t)) = self.at.get(&out.join("/")) {
                if todo.is_empty() && !follow_last {
                    break;
                }
                hops += 1;
                ensure!(hops < 64, "too many symlinks resolving {p}");
                out.pop();
                let t = t.to_string_lossy();
                if t.starts_with('/') {
                    out.clear();
                }
                todo.extend(t.split('/').filter(|c| !c.is_empty()).map(String::from).rev());
            }
        }
        Ok(out.join("/"))
    }

    fn read(&self, rel: &str) -> Option<Vec<u8>> {
        let Some(Node::File(p, off, len, _)) = self.at.get(&self.resolve(rel, true).ok()?) else { return None };
        let mut f = File::open(p.as_ref()).ok()?;
        io::Seek::seek(&mut f, io::SeekFrom::Start(*off)).ok()?;
        let mut v = vec![0; *len as usize];
        f.read_exact(&mut v).ok()?;
        Some(v)
    }
}

#[derive(Clone)]
struct Entry {
    path: String,
    kind: Kind,
    meta: Meta,
}

/// The zstd stream of a layer, and optionally the same tar uncompressed (see `plain_of`).
struct Tee {
    enc: zstd::Encoder<'static, Hashing<BufWriter<File>>>,
    plain: Option<BufWriter<File>>,
}

impl Write for Tee {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        let n = self.enc.write(b)?;
        if let Some(p) = &mut self.plain {
            p.write_all(&b[..n])?;
        }
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.enc.flush()
    }
}

/// Hands `inner` 1 MB writes (the tar writer makes 8 KB ones) and hashes them on another thread, so the
/// uncompressed digest (the diff id) costs no time next to compression.
struct ParHash<W: Write> {
    inner: W,
    buf: Vec<u8>,
    n: u64,
    tx: Option<std::sync::mpsc::SyncSender<Vec<u8>>>,
    hasher: Option<std::thread::JoinHandle<Sha256>>,
}

impl<W: Write> ParHash<W> {
    fn new(inner: W) -> Self {
        let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(8);
        let hasher = std::thread::spawn(move || {
            let mut h = Sha256::new();
            for c in rx {
                h.update(&c);
            }
            h
        });
        ParHash { inner, buf: Vec::with_capacity(1 << 20), n: 0, tx: Some(tx), hasher: Some(hasher) }
    }

    fn send(&mut self) -> io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        self.inner.write_all(&self.buf)?;
        let c = std::mem::replace(&mut self.buf, Vec::with_capacity(1 << 20));
        self.tx.as_ref().unwrap().send(c).map_err(|_| io::Error::other("hash thread gone"))
    }

    /// The digest of everything written, and `inner`.
    fn finish(mut self) -> Result<(String, W)> {
        self.send()?;
        drop(self.tx.take());
        let h = self.hasher.take().unwrap().join().map_err(|_| anyhow::anyhow!("hash thread panicked"))?;
        Ok((hex::encode(h.finalize()), self.inner))
    }
}

impl<W: Write> Write for ParHash<W> {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(b);
        self.n += b.len() as u64;
        if self.buf.len() >= 1 << 20 {
            self.send()?;
        }
        Ok(b.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

type LayerTar = tar::Builder<ParHash<Tee>>;

/// A layer's uncompressed tar, kept next to it when a later step reads the layer (a builder stage's RUN, read
/// by `COPY --from`): extract and the next incremental run then skip decompressing it.
fn plain_of(layer: &Path) -> PathBuf {
    layer.with_extension("tar")
}

/// A zstd layer being written to a temporary file in `out_dir`; `finish_layer` names it by its digest.
fn start_layer(out_dir: &Path) -> Result<(PathBuf, LayerTar)> {
    start_layer_as(out_dir, false)
}

fn start_layer_as(out_dir: &Path, plain: bool) -> Result<(PathBuf, LayerTar)> {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    fs::create_dir_all(out_dir)?;
    let tmp = out_dir.join(format!(".tmp-{}-{}", uniq(), N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
    let file = Hashing { inner: BufWriter::with_capacity(1 << 20, File::create(&tmp)?), h: Sha256::new(), n: 0 };
    let mut enc = zstd::Encoder::new(file, 3)?;
    enc.multithread(std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(4))?;
    let plain = match plain && std::env::var("IMGBUILD_NO_PLAIN").map_or(true, |v| v.is_empty() || v == "0") {
        true => Some(BufWriter::with_capacity(1 << 20, File::create(plain_of(&tmp))?)),
        false => None,
    };
    Ok((tmp, tar::Builder::new(ParHash::new(Tee { enc, plain }))))
}

fn finish_layer(b: LayerTar, tmp: PathBuf, out_dir: &Path) -> Result<crate::layer::Layer> {
    let (diff_id, Tee { enc, plain }) = b.into_inner()?.finish()?;
    let mut file = enc.finish()?;
    file.flush()?;
    let digest = hex::encode(file.h.clone().finalize());
    let size = file.n;
    drop(file);
    let path = out_dir.join(&digest);
    if let Some(mut p) = plain {
        p.flush()?;
        drop(p);
        // ponytail: a big layer (crawler's 2.7 GB browser) is not kept twice on disk; reading it decompresses
        // instead (~1.5 GB/s).
        if fs::metadata(plain_of(&tmp))?.len() > 512 << 20 {
            fs::remove_file(plain_of(&tmp))?;
        } else {
            fs::rename(plain_of(&tmp), plain_of(&path))?;
        }
    }
    fs::rename(&tmp, &path)?;
    Ok(crate::layer::Layer { path, digest: format!("sha256:{digest}"), diff_id: format!("sha256:{diff_id}"), size })
}

/// The tar entries of a layer file, from its plain sibling when it has one.
fn read_layer(path: &Path) -> Result<Box<dyn Read>> {
    match File::open(plain_of(path)) {
        Ok(f) => Ok(Box::new(io::BufReader::with_capacity(1 << 20, f))),
        Err(_) => open_layer(path),
    }
}

/// Copies a tar entry to `b` under the name `rel`.
fn append_entry<W: Write, R: Read>(b: &mut tar::Builder<W>, e: &mut tar::Entry<R>, rel: &str) -> Result<()> {
    let mut hd = e.header().clone();
    match hd.entry_type() {
        tar::EntryType::Symlink | tar::EntryType::Link => {
            let t = e.link_name()?.context("link without target")?.into_owned();
            b.append_link(&mut hd, rel, t)?;
        }
        _ => b.append_data(&mut hd, rel, e)?,
    }
    Ok(())
}

fn norm_path(p: &Path) -> String {
    let s = p.to_string_lossy();
    let mut out: Vec<&str> = vec![];
    for c in s.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    out.join("/")
}

fn is_under(p: &str, root: &str) -> bool {
    p == root || p.strip_prefix(root).is_some_and(|r| r.starts_with('/'))
}

/// `old` with `delta` (a layer of what changed since) applied: one layer, as if the step had made it all at
/// once. None when an entry of `old` is a hard link to a file `delta` replaces.
fn merge_layer(old: &Path, delta: &Path, out_dir: &Path) -> Result<Option<(crate::layer::Layer, Dirs)>> {
    let (mut paths, mut gone, mut opaque) = (std::collections::HashSet::new(), std::collections::HashSet::new(), vec![]);
    for e in tar::Archive::new(open_layer(delta)?).entries()? {
        let p = norm_path(&e?.path()?);
        let (dir, base) = p.rsplit_once('/').map_or(("", p.as_str()), |(d, b)| (d, b));
        if base == ".wh..wh..opq" {
            opaque.push(dir.to_string());
        } else if let Some(b) = base.strip_prefix(".wh.") {
            gone.insert(if dir.is_empty() { b.to_string() } else { format!("{dir}/{b}") });
        } else {
            paths.insert(p);
        }
    }
    let shadowed = |p: &str| {
        paths.contains(p) || opaque.iter().any(|o| p != o && is_under(p, o)) || {
            let mut a = p;
            loop {
                if gone.contains(a) {
                    break true;
                }
                match a.rsplit_once('/') {
                    Some((x, _)) => a = x,
                    None => break false,
                }
            }
        }
    };
    let (tmp, mut b) = start_layer(out_dir)?;
    let mut dirs = BTreeMap::new();
    for (f, is_old) in [(old, true), (delta, false)] {
        for e in tar::Archive::new(read_layer(f)?).entries()? {
            let mut e = e?;
            let p = norm_path(&e.path()?);
            if is_old && shadowed(&p) {
                continue;
            }
            let hd = e.header();
            if is_old && hd.entry_type() == tar::EntryType::Link && shadowed(&norm_path(&e.link_name()?.context("hard link without target")?)) {
                let _ = fs::remove_file(&tmp);
                return Ok(None);
            }
            if hd.entry_type() == tar::EntryType::Directory {
                dirs.insert(p.clone(), Meta { uid: hd.uid()?, gid: hd.gid()?, mode: hd.mode()?, mtime: hd.mtime()? });
            }
            let rel = e.path()?.to_string_lossy().into_owned();
            append_entry(&mut b, &mut e, &rel)?;
        }
    }
    Ok(Some((finish_layer(b, tmp, out_dir)?, Arc::new(dirs))))
}

/// Every file of a context COPY gets this mtime (2100-01-01) instead of the clamped epoch. BuildKit runs a COPY
/// after whatever is below it, so its files are newer than the base image's build artifacts and mtime-checked
/// tools rebuild them; at the epoch they look older, and cargo reused the base's placeholder build script
/// (`error: couldn't find file .../out/_connectrpc.rs`). A constant keeps the layer reproducible.
const CONTEXT_MTIME: u64 = 4_102_444_800;

fn for_context(entries: &mut [Entry]) {
    entries.iter_mut().for_each(|e| e.meta.mtime = CONTEXT_MTIME);
}

/// Puts a layer's entries in path order (a directory sorts before its contents) and clamps modification times
/// later than `epoch` to it, like BuildKit's rewrite-timestamp. The same files then give the same layer digest
/// however the source was read (a RUN's upper dir lists files in creation order, which varies between runs) and
/// whenever they were written, so an unchanged layer is already in the registry instead of being pushed again.
fn canonical(entries: &mut [Entry], epoch: u64) {
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    for e in entries {
        e.meta.mtime = e.meta.mtime.min(epoch);
    }
}

/// The Python package a path belongs to (`site-packages/<name>...`, version and suffixes dropped, so a package's
/// directory, its `-1.2.3.dist-info` and its `.libs` agree), or None outside site-packages.
fn pkg_key(path: &str) -> Option<String> {
    let i = path.find("site-packages/")?;
    if i > 0 && !path[..i].ends_with('/') {
        return None;
    }
    let comp = path[i + 14..].split('/').next().filter(|c| !c.is_empty())?;
    let name = comp.split('.').next().unwrap_or(comp);
    let cut = name.char_indices().find(|&(j, c)| c == '-' && name[j + 1..].starts_with(|d: char| d.is_ascii_digit())).map_or(name.len(), |(j, _)| j);
    Some(name[..cut].to_lowercase().replace('-', "_"))
}

/// Compiled files hash apart from their package's other files: a few `.pyc` differ between identical installs
/// (set constants marshal in hash-seed order), and that would otherwise re-push the package's whole bucket.
/// Splits one big COPY layer into up to `n + 1` layers by Python package, so that changing one dependency
/// changes (and re-pushes) only its bucket's layer instead of the whole environment. Each layer carries the
/// directories above its own files, which are the same in every build; without them Docker would create
/// them 0755 root. Not a Python environment (no site-packages path), or `n < 2`: one layer.
fn split_by_package(all: Vec<Entry>, n: usize) -> Vec<Vec<Entry>> {
    if n < 2 || !all.iter().any(|e| pkg_key(&e.path).is_some()) {
        return vec![all];
    }
    let dirs: HashMap<String, Entry> = all.iter().filter(|e| matches!(e.kind, Kind::Dir)).map(|e| (e.path.clone(), e.clone())).collect();
    let mut buckets: BTreeMap<usize, Vec<Entry>> = BTreeMap::new();
    for e in all {
        let b = match pkg_key(&e.path) {
            Some(k) => usize::from_str_radix(&h(&[&k, if e.path.ends_with(".pyc") { "pyc" } else { "" }])[..8], 16).unwrap_or(0) % n,
            None if matches!(e.kind, Kind::Dir) => continue,
            None => n,
        };
        buckets.entry(b).or_default().push(e);
    }
    for b in buckets.values_mut() {
        let mut have: std::collections::HashSet<String> = b.iter().map(|e| e.path.clone()).collect();
        let mut extra = vec![];
        for e in b.iter() {
            let mut at = e.path.len();
            while let Some(i) = e.path[..at].rfind('/') {
                at = i;
                if have.insert(e.path[..at].to_string())
                    && let Some(d) = dirs.get(&e.path[..at]) {
                        extra.push(d.clone());
                    }
            }
        }
        b.extend(extra);
    }
    buckets.into_values().collect()
}

/// A timestamp-checked `.pyc` records its source's mtime (header bytes 8..12), which for a fresh install is the
/// install time: the same environment would give a new layer digest every build (`uv sync --compile-bytecode`).
/// Sets it to the mtime the layer gives the files, which is also what makes Python accept the file.
fn stamp_pyc(buf: &mut [u8], mtime: u64) {
    if buf.len() >= 16 && u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]) & 1 == 0 {
        buf[8..12].copy_from_slice(&(mtime as u32).to_le_bytes());
    }
}

fn write_tree_to_disk(entries: &[Entry], root: &Path) -> Result<()> {
    use std::io::{Read, Seek, SeekFrom};
    use std::os::unix::fs::symlink;
    for e in entries {
        let p = root.join(&e.path);
        match &e.kind {
            Kind::Dir => {
                fs::create_dir_all(&p)?;
            }
            Kind::File(src) => {
                if let Some(parent) = p.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::copy(src, &p)?;
            }
            Kind::Slice(arc, off, len, _) => {
                if let Some(parent) = p.parent() {
                    fs::create_dir_all(parent)?;
                }
                let mut f = File::open(arc.as_ref())?;
                f.seek(SeekFrom::Start(*off))?;
                let mut out = BufWriter::with_capacity(1 << 20, File::create(&p)?);
                io::copy(&mut f.take(*len), &mut out)?;
                out.flush()?;
            }
            Kind::Link(target) => {
                if let Some(parent) = p.parent() {
                    fs::create_dir_all(parent)?;
                }
                if p.exists() {
                    fs::remove_file(&p).or_else(|_| fs::remove_dir(&p)).ok();
                }
                symlink(target, &p)?;
            }
        }
        if !matches!(e.kind, Kind::Dir) {
            fs::set_permissions(&p, fs::Permissions::from_mode(e.meta.mode & 0o7777))?;
        }
    }
    Ok(())
}

async fn export_tree(b: &Arc<B>, state: &State, prefix: &str, dest: &Path) -> Result<()> {
    let p = prefix.trim_start_matches('/').to_string();
    ensure!(!p.is_empty(), "export prefix must not be empty");
    let rf = b.materialize(state, std::slice::from_ref(&p)).await?;
    let _ = fs::remove_dir_all(dest);
    fs::create_dir_all(dest)?;
    let entries = copy_entries(
        &rf.dir,
        Some(&rf.files),
        Some(&rf.meta),
        None,
        &[format!("/{p}")],
        &format!("{}/", dest.to_string_lossy()),
        None,
        None,
    )?;
    write_tree_to_disk(&entries, dest)?;
    Ok(())
}

/// A deterministic zstd layer of `entries`, in order.
fn write_layer(entries: &[Entry], out_dir: &Path) -> Result<crate::layer::Layer> {
    let (tmp, mut b) = start_layer(out_dir)?;
    let mut open: HashMap<Arc<PathBuf>, Arc<File>> = HashMap::new();
    // Extract gives a hard link its target's slice (the same Arc): the same slice again is a hard link, as
    // BuildKit keeps it. A COPY source followed through a symlink gets an Arc of its own.
    let mut linked: HashMap<(usize, u64), &str> = HashMap::new();
    for e in entries {
        let mut hd = tar::Header::new_gnu();
        hd.set_uid(e.meta.uid);
        hd.set_gid(e.meta.gid);
        hd.set_mode(e.meta.mode & 0o7777);
        hd.set_mtime(e.meta.mtime);
        match &e.kind {
            Kind::Dir => {
                hd.set_entry_type(tar::EntryType::Directory);
                hd.set_size(0);
                b.append_data(&mut hd, format!("{}/", e.path), io::empty())?;
            }
            Kind::File(src) => {
                let f = File::open(src).with_context(|| src.display().to_string())?;
                hd.set_entry_type(tar::EntryType::Regular);
                hd.set_size(f.metadata()?.len());
                b.append_data(&mut hd, &e.path, f)?;
            }
            Kind::Link(target) => {
                hd.set_entry_type(tar::EntryType::Symlink);
                hd.set_size(0);
                b.append_link(&mut hd, &e.path, target)?;
            }
            Kind::Slice(p, off, _, _) if linked.contains_key(&(Arc::as_ptr(p) as usize, *off)) => {
                hd.set_entry_type(tar::EntryType::Link);
                hd.set_size(0);
                b.append_link(&mut hd, &e.path, linked[&(Arc::as_ptr(p) as usize, *off)])?;
            }
            Kind::Slice(p, off, len, _) => {
                linked.insert((Arc::as_ptr(p) as usize, *off), &e.path);
                let f = match open.get(p) {
                    Some(f) => f.clone(),
                    None => {
                        let f = Arc::new(File::open(p.as_ref())?);
                        open.insert(p.clone(), f.clone());
                        f
                    }
                };
                hd.set_entry_type(tar::EntryType::Regular);
                hd.set_size(*len);
                // One read a file: io::copy would take 8 KB at a time, a syscall each (0.3 s for a 450 MB venv).
                let mut buf = vec![0u8; *len as usize];
                std::os::unix::fs::FileExt::read_exact_at(f.as_ref(), &mut buf, *off)?;
                if e.path.ends_with(".pyc") {
                    stamp_pyc(&mut buf, e.meta.mtime);
                }
                b.append_data(&mut hd, &e.path, buf.as_slice())?;
            }
        }
    }
    finish_layer(b, tmp, out_dir)
}

/// The container that reads other containers' overlay upper directories for `layer_from_upper`.
const UPPER_HELPER: &str = "imgbuild-upper";

/// Whether `UPPER_HELPER` runs on this daemon (and how it hands over tars), starting it if not: privileged with the daemon's PID namespace,
/// it reaches the daemon's snapshots through /proc/1/root. The image needs GNU tar (for xattrs).
fn upper_helper(docker: &dyn Fn() -> Command, daemon: Option<&str>) -> Option<Xfer> {
    static UP: std::sync::Mutex<BTreeMap<String, Option<Xfer>>> = std::sync::Mutex::new(BTreeMap::new());
    if std::env::var("IMGBUILD_UPPER").is_ok_and(|v| v == "0") {
        return None;
    }
    let mut up = UP.lock().unwrap();
    if let Some(x) = up.get(daemon.unwrap_or("")) {
        return x.clone();
    }
    let quiet = |c: &mut Command| c.stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success());
    // One fixed directory, mounted at /xfer: it is the same directory here only where the daemon shares
    // this machine's files (Docker Desktop shares the temp dirs).
    let d = std::env::temp_dir().join("imgbuild-xfer");
    let shared = fs::create_dir_all(&d).ok().and_then(|_| d.canonicalize().ok());
    let mount = shared.as_ref().map(|d| format!("{}:/xfer", d.display()));
    // What the last probe found, for 10 minutes (each probe is a docker round trip, ~0.1 s on Docker Desktop);
    // `layer_from_upper` forgets it when the helper fails.
    let memo = helper_memo(daemon);
    let known = memo.as_ref().filter(|m| fs::metadata(m).ok().and_then(|x| x.modified().ok()?.elapsed().ok()).is_some_and(|a| a.as_secs() < 600))
        .and_then(|m| fs::read_to_string(m).ok());
    if let Some(k) = known {
        crate::prof::start_sampler(docker, UPPER_HELPER);
        let x = Some(Xfer(shared.clone().filter(|d| k == d.to_string_lossy())));
        up.insert(daemon.unwrap_or("").to_string(), x.clone());
        return x;
    }
    let probe = format!(".probe-{}", uniq());
    let check = match mount {
        Some(_) => format!("test -d /xfer && {{ touch /xfer/{probe} || true; }}"),
        None => "test -d /".into(),
    };
    let ok = quiet(docker().args(["exec", UPPER_HELPER, "sh", "-c", &check])) || {
        let image = std::env::var("IMGBUILD_HELPER_IMAGE").unwrap_or_else(|_| "debian:bookworm-slim".into());
        quiet(docker().args(["rm", "-f", UPPER_HELPER]));
        let mut run = docker();
        run.args(["run", "-d", "--name", UPPER_HELPER, "--privileged", "--pid=host", "--restart", "unless-stopped", "--entrypoint", "sleep"]);
        if let Some(m) = &mount {
            run.args(["--volume", m]);
        }
        quiet(run.args([&image, "infinity"])) && quiet(docker().args(["exec", UPPER_HELPER, "tar", "--xattrs", "--version"]))
    };
    if ok {
        crate::prof::start_sampler(docker, UPPER_HELPER);
    }
    let x = ok.then(|| {
        let seen = shared.filter(|d| (d.join(&probe).exists() || quiet(docker().args(["exec", UPPER_HELPER, "touch", &format!("/xfer/{probe}")]))) && fs::remove_file(d.join(&probe)).is_ok());
        Xfer(seen)
    });
    if let (Some(m), Some(x)) = (&memo, &x) {
        let _ = fs::create_dir_all(m.parent().unwrap()).and_then(|_| put_file(m, x.0.as_ref().map_or(String::new(), |d| d.to_string_lossy().into_owned()).as_bytes()));
    }
    up.insert(daemon.unwrap_or("").to_string(), x.clone());
    x
}

fn helper_memo(daemon: Option<&str>) -> Option<PathBuf> {
    node_cache("helper").map(|d| d.join(&h(&[daemon.unwrap_or("")])[..16]))
}

/// Small blob `digest` (a manifest or config of an image the daemon pulled) from the daemon's containerd
/// content store through `UPPER_HELPER`, avoiding a registry round trip for each manifest.
pub fn local_content(digest: &str) -> Option<Bytes> {
    let hex = digest.strip_prefix("sha256:").filter(|h| h.len() == 64 && h.bytes().all(|c| c.is_ascii_hexdigit()))?;
    let docker = || Command::new("docker");
    upper_helper(&docker, None)?;
    let paths = ["desktop-containerd/daemon", "docker/containerd/daemon", "containerd"]
        .map(|r| format!("/proc/1/root/var/lib/{r}/io.containerd.content.v1.content/blobs/sha256/{hex}"));
    let script = format!("for f in {}; do [ -f $f ] && [ $(stat -c %s $f) -lt 1048576 ] && exec cat $f; done; exit 3", paths.join(" "));
    let o = docker().args(["exec", UPPER_HELPER, "sh", "-c", &script]).stderr(Stdio::null()).output().ok()?;
    o.status.success().then(|| Bytes::from(o.stdout))
}

/// Blob `digest` (a layer of an image the daemon pulled) from the daemon's containerd content store into
/// `path`, checked against its digest: the same as a registry download, without the network.
pub fn local_content_file(digest: &str, path: &Path) -> bool {
    let Some(hex) = digest.strip_prefix("sha256:").filter(|h| h.len() == 64 && h.bytes().all(|c| c.is_ascii_hexdigit())) else { return false };
    let docker = || Command::new("docker");
    let Some(Xfer(Some(dir))) = upper_helper(&docker, None) else { return false };
    let name = format!(".blob-{}-{hex}", uniq());
    let paths = ["desktop-containerd/daemon", "docker/containerd/daemon", "containerd"]
        .map(|r| format!("/proc/1/root/var/lib/{r}/io.containerd.content.v1.content/blobs/sha256/{hex}"));
    let script = format!("for f in {}; do [ -f $f ] && exec cp $f /xfer/{name}; done; exit 3", paths.join(" "));
    let ok = docker().args(["exec", UPPER_HELPER, "sh", "-c", &script]).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success());
    let f = dir.join(&name);
    let good = ok && (|| -> Result<bool> {
        let (mut h, mut r, mut buf) = (Sha256::new(), File::open(&f)?, vec![0u8; 1 << 20]);
        loop {
            match r.read(&mut buf)? {
                0 => break,
                n => h.update(&buf[..n]),
            }
        }
        Ok(hex::encode(h.finalize()) == hex && (fs::rename(&f, path).is_ok() || fs::copy(&f, path).is_ok()))
    })().unwrap_or(false);
    let _ = fs::remove_file(&f);
    good
}

/// How `layer_from_upper` gets the helper's tar: as a file in this directory, which the helper sees as
/// /xfer (Docker Desktop's file sharing moves ~680 MB/s in 1 MB writes, its exec stream ~140 MB/s), or else streamed.
#[derive(Clone)]
struct Xfer(Option<PathBuf>);

/// A tar of one empty file `marker` at /, for `docker cp - ctr:/`: `layer_from_upper` finds the container's
/// upper directory by it.
fn marker_tar(marker: &str) -> Result<Vec<u8>> {
    let mut b = tar::Builder::new(Vec::new());
    let mut hd = tar::Header::new_gnu();
    hd.set_mode(0o600);
    hd.set_size(0);
    b.append_data(&mut hd, marker, io::empty())?;
    Ok(b.into_inner()?)
}

/// What a RUN changed in container `name` (stopped; it has the empty file /`marker`), as a layer: its
/// overlay upper directory, read by `UPPER_HELPER` at disk speed, with overlay's whiteouts and opaque
/// directories turned into OCI ones. Mounts' contents are not in it; directories made only to hold a mount
/// target are dropped, as BuildKit does. None when the upper directory is not found or holds something
/// this does not convert (overlay metacopy or redirect, device files); the caller then reads the container.
fn layer_from_upper(docker: &dyn Fn() -> Command, xfer: &Xfer, marker: &str, out_dir: &Path, mounts: &[String], plain: bool, epoch: u64) -> Result<Option<(crate::layer::Layer, Dirs)>> {
    let roots = [
        "/proc/1/root/var/lib/desktop-containerd/daemon/io.containerd.snapshotter.v1.overlayfs/snapshots/*/fs",
        "/proc/1/root/var/lib/docker/containerd/daemon/io.containerd.snapshotter.v1.overlayfs/snapshots/*/fs",
        "/proc/1/root/var/lib/docker/overlay2/*/diff",
    ];
    let globs: Vec<String> = roots.iter().map(|r| format!("{r}/{marker}")).collect();
    let out = if xfer.0.is_some() { format!("/xfer/{marker}.tar") } else { "-".into() };
    let script = format!(
        // The marker goes once read, so a seed image committed from the container does not carry it.
        "for f in {}; do [ -e \"$f\" ] && {{ tar -b 2048 --xattrs {} {} --exclude=./{marker} -C \"${{f%/{marker}}}\" -cf {out} .; s=$?; rm -f \"$f\"; exit $s; }}; done; exit 3",
        globs.join(" "),
        // A pushed layer lists files in name order with mtimes clamped to the epoch (see `canonical`), so the same
        // files give the same digest; an intermediate layer is not pushed.
        if plain { String::new() } else { format!("--pax-option=delete=atime,delete=ctime --sort=name --mtime=@{epoch} --clamp-mtime") },
        // Not overlay's other xattrs: binary values (origin) are more than the tar crate's pax parser takes.
        ["opaque", "metacopy", "redirect"].iter().flat_map(|a| [format!("--xattrs-include=trusted.overlay.{a}"), format!("--xattrs-include=user.overlay.{a}")])
            .chain(["--xattrs-include=security.capability".to_string()]).collect::<Vec<_>>().join(" "));
    let mut child = docker().args(["exec", UPPER_HELPER, "sh", "-c", &script]).stdout(Stdio::piped()).stderr(Stdio::null()).spawn()?;
    let input: Box<dyn Read> = match &xfer.0 {
        Some(d) => {
            let f = d.join(format!("{marker}.tar"));
            let t = Instant::now();
            let st = child.wait()?;
            if std::env::var_os("IMGBUILD_TRACE").is_some() {
                eprintln!("  trace upper dir: tar to the shared dir {:.2}s", t.elapsed().as_secs_f64());
            }
            let r = fs::File::open(&f);
            // Unlinked now: the open file is still read to its end, and nothing is left behind.
            let _ = fs::remove_file(&f);
            match r {
                Ok(r) if st.success() => Box::new(r),
                _ => {
                    // Exit 3: no such upper dir. Anything else may be the helper gone: probe it next time.
                    if st.code() != Some(3) {
                        let _ = fs::remove_dir_all(node_cache("helper").unwrap_or_default());
                    }
                    return Ok(None);
                }
            }
        }
        None => Box::new(child.stdout.take().unwrap()),
    };
    let (tmp, mut b) = start_layer_as(out_dir, plain)?;
    let mut dirs = BTreeMap::new();
    let mut written = std::collections::HashSet::new();
    type Pending = (String, Meta, Vec<(String, Vec<u8>)>, bool);
    fn put_dir(b: &mut LayerTar, rel: &str, m: Meta, xattrs: &[(String, Vec<u8>)], opaque: bool) -> Result<()> {
        if !xattrs.is_empty() {
            b.append_pax_extensions(xattrs.iter().map(|(k, v)| (k.as_str(), v.as_slice())))?;
        }
        let mut hd = tar::Header::new_gnu();
        hd.set_uid(m.uid);
        hd.set_gid(m.gid);
        hd.set_mode(m.mode);
        hd.set_mtime(m.mtime);
        hd.set_entry_type(tar::EntryType::Directory);
        hd.set_size(0);
        b.append_data(&mut hd, format!("{rel}/"), io::empty())?;
        if opaque {
            let mut hd = tar::Header::new_gnu();
            hd.set_mode(0o644);
            hd.set_size(0);
            b.append_data(&mut hd, format!("{rel}/.wh..wh..opq"), io::empty())?;
        }
        Ok(())
    }
    let ok = (|| -> Result<bool> {
        let mut pending: Vec<Pending> = vec![];
        let mut ar = tar::Archive::new(io::BufReader::with_capacity(1 << 20, input));
        for e in ar.entries()? {
            let mut e = e?;
            let rel = norm_path(&e.path()?);
            if rel.is_empty() || mounts.iter().any(|m| is_under(&rel, m)) {
                continue;
            }
            let (mut xattrs, mut opaque) = (vec![], false);
            for x in e.pax_extensions()?.into_iter().flatten() {
                let x = x?;
                let (k, v) = (x.key()?.to_string(), x.value_bytes().to_vec());
                let Some(a) = k.strip_prefix("SCHILY.xattr.") else { continue };
                match a.split_once("overlay.") {
                    Some((ns, "opaque")) if ns == "trusted." || ns == "user." => opaque = v == b"y",
                    Some((_, "metacopy" | "redirect")) => return Ok(false),
                    Some(_) => {}
                    None => xattrs.push((k, v)),
                }
            }
            let src = e.header();
            let (kind, m) = (src.entry_type(), Meta { uid: src.uid()?, gid: src.gid()?, mode: src.mode()? & 0o7777, mtime: src.mtime()? });
            if kind == tar::EntryType::Directory && mounts.iter().any(|t| t.starts_with(&format!("{rel}/"))) {
                pending.push((rel, m, xattrs, opaque));
                continue;
            }
            // A directory that holds a mount target goes in only once something else is in it.
            let (now, later): (Vec<Pending>, Vec<Pending>) = pending.into_iter().partition(|(p, ..)| rel.starts_with(&format!("{p}/")));
            pending = later;
            for (p, pm, px, po) in now {
                put_dir(&mut b, &p, pm, &px, po)?;
                dirs.insert(p, pm);
            }
            let mut hd = tar::Header::new_gnu();
            hd.set_uid(m.uid);
            hd.set_gid(m.gid);
            hd.set_mode(m.mode);
            hd.set_mtime(m.mtime);
            match kind {
                tar::EntryType::Directory => {
                    put_dir(&mut b, &rel, m, &xattrs, opaque)?;
                    dirs.insert(rel.clone(), m);
                }
                tar::EntryType::Char if src.device_major()? == Some(0) && src.device_minor()? == Some(0) => {
                    let wh = match rel.rsplit_once('/') {
                        Some((d, base)) => format!("{d}/.wh.{base}"),
                        None => format!(".wh.{rel}"),
                    };
                    let mut hd = tar::Header::new_gnu();
                    hd.set_mode(0o644);
                    hd.set_size(0);
                    b.append_data(&mut hd, wh, io::empty())?;
                }
                tar::EntryType::Regular | tar::EntryType::Continuous | tar::EntryType::GNUSparse => {
                    if !xattrs.is_empty() {
                        b.append_pax_extensions(xattrs.iter().map(|(k, v)| (k.as_str(), v.as_slice())))?;
                    }
                    hd.set_entry_type(tar::EntryType::Regular);
                    hd.set_size(e.size());
                    b.append_data(&mut hd, &rel, &mut e)?;
                }
                tar::EntryType::Symlink => {
                    let t = e.link_name()?.context("symlink without target")?.into_owned();
                    hd.set_entry_type(kind);
                    hd.set_size(0);
                    b.append_link(&mut hd, &rel, t)?;
                }
                tar::EntryType::Link => {
                    let t = norm_path(&e.link_name()?.context("hard link without target")?);
                    if !written.contains(&t) {
                        return Ok(false);
                    }
                    hd.set_entry_type(kind);
                    hd.set_size(0);
                    b.append_link(&mut hd, &rel, t)?;
                }
                _ => return Ok(false),
            }
            written.insert(rel);
        }
        // tar pads its output to whole records (1 MB here) after the end marker: unread, the helper dies of SIGPIPE.
        io::copy(&mut ar.into_inner(), &mut io::sink())?;
        Ok(true)
    })();
    let st = child.wait()?;
    if !matches!(ok, Ok(true)) || !st.success() {
        let _ = fs::remove_file(&tmp);
        let _ = fs::remove_file(plain_of(&tmp));
        if std::env::var_os("IMGBUILD_TRACE").is_some() {
            eprintln!("  trace upper dir not used ({st}, {:?})", ok.as_ref().map_err(|e| e.to_string()));
        }
        return Ok(None);
    }
    Ok(Some((finish_layer(b, tmp, out_dir)?, Arc::new(dirs))))
}

/// What `layer_from_container` keeps of a `docker cp` of a path.
enum Take {
    /// the whole tree
    Whole,
    /// the path's own entry
    Own,
    /// these paths in it, and everything under the second list's paths
    Pick(std::collections::HashSet<String>, Vec<String>),
}

/// What a RUN changed in container `name`, as a layer: the paths `docker diff` lists, read with `docker cp`
/// (an added path's whole tree, or a changed path's own entry) instead of a `docker export` of the whole
/// filesystem, which goes at ~60 MB/s through Docker Desktop. `mounts`: RUN mount targets, whose contents
/// are not the image's. `own_dirs`: read a changed directory's own entry (its mode, owner, mtime), which
/// makes dockerd send its whole tree; without, the layer has no entry for it. A directory that only holds
/// a mount target never gets one. None when the layer cannot be made this way (a hard link to a file the
/// RUN did not touch, a device file): the caller commits instead.
fn layer_from_container(docker: &dyn Fn() -> Command, name: &str, out_dir: &Path, mounts: &[String], own_dirs: bool, plain: bool) -> Result<Option<(crate::layer::Layer, Dirs)>> {
    let (mut kinds, mut gone) = (BTreeMap::new(), vec![]);
    for l in sh(docker().args(["diff", name]))?.lines() {
        let Some((kind, p)) = l.split_once(' ') else { continue };
        let p = p.trim_matches('/').to_string();
        // `.{name}` is the marker `layer_from_upper` looks for.
        if !p.is_empty() && p != format!(".{name}") {
            if kind == "D" { gone.push(p) } else { kinds.insert(p, kind == "A"); }
        }
    }
    let trace = std::env::var_os("IMGBUILD_TRACE").is_some();
    let t0 = Instant::now();
    gone.sort();
    for p in &gone {
        let d = format!("{p}/");
        if kinds.keys().any(|k| k.starts_with(&d)) {
            kinds.entry(p.clone()).or_insert(false);
        }
    }
    let under = is_under;
    let mut whole: std::collections::HashSet<String> = Default::default();
    let (mut reads, mut dir_reads) = (vec![], vec![]);
    for (p, added) in &kinds {
        let mut anc = p.as_str();
        let covered = loop {
            match anc.rsplit_once('/') {
                Some((a, _)) if whole.contains(a) => break true,
                Some((a, _)) => anc = a,
                None => break false,
            }
        };
        if covered || mounts.iter().any(|m| p != m && under(p, m)) {
            continue;
        }
        let holds_mount = mounts.iter().any(|m| under(m, p));
        // A directory made only to hold a mount target goes, as BuildKit removes it.
        let mount_only = |k: &String| mounts.iter().any(|m| under(k, m) || under(m, k));
        if *added && holds_mount && kinds.range(p.clone()..).take_while(|(k, _)| under(k, p)).all(|(k, _)| mount_only(k)) {
            continue;
        }
        let all = *added && !holds_mount;
        if all {
            whole.insert(p.clone());
            reads.push((p.clone(), Take::Whole));
            continue;
        }
        let pd = format!("{p}/");
        let is_dir = kinds.range(pd.clone()..).next().is_some_and(|(k, _)| k.starts_with(&pd)) || gone.iter().any(|g| g.starts_with(&pd));
        if !is_dir {
            reads.push((p.clone(), Take::Own));
        } else if own_dirs && !holds_mount {
            dir_reads.push((p.clone(), Take::Own));
        }
    }
    // Many single paths under one directory (a RUN that wrote hundreds of .pyc files into the existing
    // __pycache__ directories of /usr/lib/python3.x) are one copy of it that keeps just those: each copy is a
    // docker round trip. ponytail: the directory is the path's first three components, or its parent if
    // shallower; that copy reads the whole tree, which is fine for a language's library directory.
    let leafy = |p: &str| {
        let pd = format!("{p}/");
        !kinds.range(pd.clone()..).next().is_some_and(|(k, _)| k.starts_with(&pd))
    };
    // Changed directories last and deepest first: a copy that stops after a directory's own entry still
    // holds the container until dockerd has sent the rest of the tree.
    dir_reads.reverse();
    reads.extend(dir_reads);
    let mut by_parent: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (i, (p, t)) in reads.iter().enumerate() {
        if let Some((parent, _)) = p.rsplit_once('/').filter(|_| matches!(t, Take::Own) || leafy(p)) {
            let top = parent.split('/').take(3).collect::<Vec<_>>().join("/");
            by_parent.entry(top).or_default().push(i);
        }
    }
    let mut picked = std::collections::HashSet::new();
    let mut picks = vec![];
    for (parent, idx) in by_parent {
        if idx.len() >= 8 && !mounts.iter().any(|m| under(m, &parent)) {
            let set = idx.iter().map(|&i| reads[i].0.clone()).collect();
            picked.extend(idx);
            picks.push((parent, Take::Pick(set, vec![])));
        }
    }
    let mut reads: Vec<(String, Take)> = reads.into_iter().enumerate().filter(|(i, _)| !picked.contains(i)).map(|(_, r)| r).collect();
    // Before the own-entry reads, which leave the container busy.
    let first_own = reads.iter().position(|(_, t)| matches!(t, Take::Own) ).unwrap_or(reads.len());
    reads.splice(first_own..first_own, picks);
    // A changed top-level directory's own entry streams its whole tree anyway (dockerd sends all of it even
    // when the reader stops early), so everything wanted from under it comes from that one read.
    if own_dirs {
        let tops: std::collections::HashSet<String> = reads.iter().filter(|(p, t)| matches!(t, Take::Own) && !p.contains('/')).map(|(p, _)| p.clone()).collect();
        let top_of = |p: &str| p.split('/').next().unwrap_or("").to_string();
        let mut groups: BTreeMap<String, (std::collections::HashSet<String>, Vec<String>)> = BTreeMap::new();
        reads.retain(|(p, t)| {
            let top = top_of(p);
            if !tops.contains(&top) {
                return true;
            }
            let g = groups.entry(top).or_default();
            match t {
                Take::Whole => g.1.push(p.clone()),
                Take::Own => {
                    g.0.insert(p.clone());
                }
                Take::Pick(set, whole) => {
                    g.0.extend(set.iter().cloned());
                    g.1.extend(whole.iter().cloned());
                }
            }
            false
        });
        reads.extend(groups.into_iter().map(|(top, (set, whole))| (top, Take::Pick(set, whole))));
    }
    let (tmp, mut b) = start_layer_as(out_dir, plain)?;
    // Whiteouts first: our own extract applies a layer's entries in order. A deleted path that has entries
    // again was replaced as a whole (overlay reports an opaque directory as deleted), so it hides the lower
    // directory's contents instead.
    for p in &gone {
        let under_p = format!("{p}/");
        let wh = if kinds.contains_key(p) || kinds.keys().any(|k| k.starts_with(&under_p)) {
            format!("{p}/.wh..wh..opq")
        } else {
            match p.rsplit_once('/') {
                Some((dir, base)) => format!("{dir}/.wh.{base}"),
                None => format!(".wh.{p}"),
            }
        };
        let mut hd = tar::Header::new_gnu();
        hd.set_mode(0o644);
        hd.set_size(0);
        b.append_data(&mut hd, wh, io::empty())?;
    }
    // One copy at a time, in path order (a directory before what is in it): dockerd holds the container's
    // lock for the whole of a copy, so a second one waits for the first to be read to its end.
    let norm = norm_path;
    let mut dirs = BTreeMap::new();
    let mut written = std::collections::HashSet::new();
    let ok = (|| -> Result<bool> {
        for (p, take) in &reads {
            let mut child = docker().args(["cp", &format!("{name}:/{p}"), "-"]).stdout(Stdio::piped()).stderr(Stdio::null()).spawn()?;
            // `docker cp` names entries from the copied path's base name.
            if trace {
                eprintln!("  trace {:>6}ms cp {p} ({}), {} paths changed", t0.elapsed().as_millis(), match take { Take::Whole => "whole".into(), Take::Own => "own".into(), Take::Pick(s, w) => format!("{} paths and {} trees of it", s.len(), w.len()) }, kinds.len());
            }
            let parent = p.rsplit_once('/').map(|(a, _)| format!("{a}/")).unwrap_or_default();
            let mut ar = tar::Archive::new(io::BufReader::with_capacity(1 << 20, child.stdout.take().unwrap()));
            for e in ar.entries()? {
                let mut e = e?;
                let rel = format!("{parent}{}", norm(&e.path()?));
                if mounts.iter().any(|m| rel != *m && under(&rel, m)) || matches!(take, Take::Pick(s, w) if !s.contains(&rel) && !w.iter().any(|w| rel == *w || under(&rel, w))) {
                    continue;
                }
                let src = e.header();
                let (kind, m) = (src.entry_type(), Meta { uid: src.uid()?, gid: src.gid()?, mode: src.mode()?, mtime: src.mtime()? });
                let mut hd = tar::Header::new_gnu();
                hd.set_uid(m.uid);
                hd.set_gid(m.gid);
                hd.set_mode(m.mode);
                hd.set_mtime(m.mtime);
                let xattrs: Vec<(String, Vec<u8>)> = e.pax_extensions()?.into_iter().flatten().flatten()
                    .filter_map(|x| Some((x.key().ok()?.to_string(), x.value_bytes().to_vec())))
                    .filter(|(k, _)| k.starts_with("SCHILY.xattr."))
                    .collect();
                if !xattrs.is_empty() {
                    b.append_pax_extensions(xattrs.iter().map(|(k, v)| (k.as_str(), v.as_slice())))?;
                }
                match kind {
                    tar::EntryType::Directory => {
                        hd.set_entry_type(kind);
                        hd.set_size(0);
                        b.append_data(&mut hd, format!("{rel}/"), io::empty())?;
                        dirs.insert(rel.clone(), m);
                    }
                    tar::EntryType::Regular | tar::EntryType::Continuous | tar::EntryType::GNUSparse => {
                        hd.set_entry_type(tar::EntryType::Regular);
                        hd.set_size(e.size());
                        b.append_data(&mut hd, &rel, &mut e)?;
                    }
                    tar::EntryType::Symlink => {
                        let t = e.link_name()?.context("symlink without target")?.into_owned();
                        hd.set_entry_type(kind);
                        hd.set_size(0);
                        b.append_link(&mut hd, &rel, t)?;
                    }
                    tar::EntryType::Link => {
                        let t = format!("{parent}{}", norm(&e.link_name()?.context("hard link without target")?));
                        if !written.contains(&t) {
                            return Ok(false);
                        }
                        hd.set_entry_type(kind);
                        hd.set_size(0);
                        b.append_link(&mut hd, &rel, t)?;
                    }
                    _ => return Ok(false),
                }
                written.insert(rel);
                if matches!(take, Take::Own) {
                    break;
                }
            }
            drop(ar);
            let own = matches!(take, Take::Own);
            if own {
                let _ = child.kill();
            }
            let st = child.wait()?;
            ensure!(st.success() || own, "docker cp {name}:/{p} failed");
        }
        Ok(true)
    })();
    if !matches!(ok, Ok(true)) {
        let _ = fs::remove_file(&tmp);
        return ok.map(|_| None);
    }
    if trace {
        eprintln!("  trace {:>6}ms copies read", t0.elapsed().as_millis());
    }
    Ok(Some((finish_layer(b, tmp, out_dir)?, Arc::new(dirs))))
}

/// A filesystem extracted from image layers, with the ownership and modes the unprivileged extraction lost.
struct Rootfs {
    dir: PathBuf,
    meta: HashMap<String, Meta>,
    files: Files,
}

fn open_layer(path: &Path) -> Result<Box<dyn Read>> {
    let mut f = File::open(path)?;
    let mut magic = [0u8; 4];
    let n = f.read(&mut magic)?;
    drop(f);
    let f = io::BufReader::with_capacity(1 << 20, File::open(path)?);
    Ok(match &magic[..n] {
        [0x1f, 0x8b, ..] => Box::new(flate2::read::GzDecoder::new(f)),
        [0x28, 0xb5, 0x2f, 0xfd] => Box::new(zstd::Decoder::new(f)?),
        _ => Box::new(f),
    })
}

/// Applies layers in order (whiteouts included). With `prefixes`, only entries under them (and symlinks) are kept.
/// The entries of `layer` an extract for `prefixes` uses, as an uncompressed tar at `out`. Whiteouts and
/// symlinks always: a later path may resolve through them.
fn layer_subset(layer: &Path, prefixes: &[String], out: &Path) -> Result<()> {
    let mut prefixes = prefixes.to_vec();
    loop {
        let keep = |rel: &str| {
            prefixes.is_empty()
                || prefixes.iter().any(|p| p.is_empty() || rel == p || rel.starts_with(&format!("{p}/")) || p.starts_with(&format!("{rel}/")))
        };
        let tmp = out.with_extension(format!("tmp{}", uniq()));
        let mut b = tar::Builder::new(BufWriter::with_capacity(1 << 20, File::create(&tmp)?));
        let (mut kept, mut missing) = (std::collections::HashSet::new(), vec![]);
        for e in tar::Archive::new(open_layer(layer)?).entries()? {
            let mut e = e?;
            let raw = e.path()?.to_string_lossy().to_string();
            let rel = raw.split('/').filter(|c| !c.is_empty() && *c != ".").collect::<Vec<_>>().join("/");
            let wh = rel.rsplit('/').next().is_some_and(|n| n.starts_with(".wh."));
            if wh || e.header().entry_type().is_symlink() || keep(&rel) {
                if e.header().entry_type() == tar::EntryType::Link {
                    let t = norm_path(&e.link_name()?.context("hard link without target")?);
                    if !kept.contains(&t) {
                        missing.push(t);
                    }
                }
                append_entry(&mut b, &mut e, &raw)?;
                kept.insert(rel);
            }
        }
        b.into_inner()?.flush()?;
        // A kept hard link whose target is outside the prefixes (cargo links target/release/x to deps/x-<hash>)
        // needs the target too: one more pass with it kept.
        if missing.is_empty() {
            fs::rename(&tmp, out)?;
            return Ok(());
        }
        prefixes.extend(missing);
    }
}

/// How one `COPY --from` maps its single directory source `src` to `dest` (both normalized), for `copy_delta`.
struct CopyMap {
    src: String,
    dest: String,
    chown: Option<(u64, u64)>,
    chmod: Option<u32>,
    /// `dest` did not exist before the COPY, so all under it is the copy's and whiteouts there are safe.
    fresh: bool,
}

/// Per added source layer its delta, and per removed one whether it touched the copy.
type Deltas = (Vec<(String, Delta)>, Vec<bool>);
/// A RUN's layer (none when it changed nothing) and its timings.
type RunLayer = (Option<(crate::layer::Layer, Dirs)>, [f64; 3]);

enum Delta {
    /// The layer may change the copy in a way `copy_delta` does not follow.
    Unsafe,
    Untouched,
    Layer(crate::layer::Layer, Dirs),
}

/// What a source stage layer does to a `COPY --from` of `prefixes`, and with `map`, the same change as a
/// layer on top of the earlier copy.
fn copy_delta(layer: &Path, prefixes: &[String], map: Option<&CopyMap>, out_dir: &Path) -> Result<Delta> {
    let join = |a: &str, b: &str| if a.is_empty() { b.to_string() } else if b.is_empty() { a.to_string() } else { format!("{a}/{b}") };
    let mut out: Option<(PathBuf, LayerTar)> = None;
    let mut dirs = BTreeMap::new();
    for e in tar::Archive::new(read_layer(layer)?).entries()? {
        let mut e = e?;
        let r = norm_path(&e.path()?);
        let (dir, name) = r.rsplit_once('/').unwrap_or(("", r.as_str()));
        let wh = name.strip_prefix(".wh.");
        let opaque = wh == Some(".wh..opq");
        let target = match wh {
            Some(_) if opaque => dir.to_string(),
            Some(x) => join(dir, x),
            None => r.clone(),
        };
        let ty = e.header().entry_type();
        // A directory entry above a source keeps what is under it; anything else there moves the source.
        if prefixes.iter().any(|p| is_under(p, &target) && (p != &target || opaque)) {
            if wh.is_none() && ty.is_dir() {
                continue;
            }
            return Ok(Delta::Unsafe);
        }
        if !prefixes.iter().any(|p| is_under(&target, p)) {
            continue;
        }
        let Some(m) = map else { return Ok(Delta::Unsafe) };
        // The source directory's own entry: the destination keeps what it was given when first copied.
        if target == m.src && ty.is_dir() && wh.is_none() {
            continue;
        }
        if (target == m.src && !ty.is_dir()) || (wh.is_some() && !m.fresh) {
            return Ok(Delta::Unsafe);
        }
        let to = |t: &str| join(&m.dest, t[m.src.len()..].trim_start_matches('/'));
        let path = match wh {
            Some(_) => join(&to(dir), name),
            None => to(&r),
        };
        let (_, b) = match &mut out {
            Some(o) => o,
            None => out.insert(start_layer(out_dir)?),
        };
        let mut hd = tar::Header::new_gnu();
        hd.set_size(0);
        if wh.is_some() {
            hd.set_entry_type(tar::EntryType::Regular);
            hd.set_mode(0o644); hd.set_uid(0); hd.set_gid(0); hd.set_mtime(0);
            b.append_data(&mut hd, &path, io::empty())?;
            continue;
        }
        let src_hd = e.header().clone();
        let (uid, gid) = m.chown.unwrap_or((src_hd.uid()?, src_hd.gid()?));
        let meta = Meta { uid, gid, mode: m.chmod.unwrap_or(src_hd.mode()? & 0o7777), mtime: src_hd.mtime()? };
        hd.set_uid(meta.uid);
        hd.set_gid(meta.gid);
        hd.set_mode(meta.mode);
        hd.set_mtime(meta.mtime);
        match (wh, ty) {
            (None, tar::EntryType::Directory) => {
                hd.set_entry_type(tar::EntryType::Directory);
                b.append_data(&mut hd, format!("{path}/"), io::empty())?;
                dirs.insert(path, meta);
            }
            (None, tar::EntryType::Regular | tar::EntryType::Continuous) => {
                hd.set_entry_type(tar::EntryType::Regular);
                hd.set_size(e.size());
                b.append_data(&mut hd, &path, &mut e)?;
            }
            (None, tar::EntryType::Symlink) => {
                hd.set_entry_type(tar::EntryType::Symlink);
                let t = e.link_name()?.context("symlink without target")?.into_owned();
                b.append_link(&mut hd, &path, t)?;
            }
            (None, tar::EntryType::Link) => {
                let t = norm_path(&e.link_name()?.context("hard link without target")?);
                if !is_under(&t, &m.src) {
                    return Ok(Delta::Unsafe);
                }
                hd.set_entry_type(tar::EntryType::Link);
                b.append_link(&mut hd, &path, to(&t))?;
            }
            _ => return Ok(Delta::Unsafe),
        }
    }
    Ok(match out {
        Some((tmp, b)) => Delta::Layer(finish_layer(b, tmp, out_dir)?, Arc::new(dirs)),
        None => Delta::Untouched,
    })
}

/// What a COPY result entry is, as BuildKit's COPY cache checksum sees it: type, mode, owner, size, link
/// target and content, not mtime.
type Sig = (u8, u32, u64, u64, u64, Option<PathBuf>, Vec<u8>);

fn sig<R: Read>(e: &mut tar::Entry<R>) -> Result<Sig> {
    let hd = e.header().clone();
    let (mut h, mut buf) = (Sha256::new(), vec![0u8; 1 << 16]);
    loop {
        match e.read(&mut buf)? {
            0 => break,
            n => h.update(&buf[..n]),
        }
    }
    Ok((hd.entry_type().as_byte(), hd.mode()? & 0o7777, hd.uid()?, hd.gid()?, hd.size()?, hd.link_name()?.map(|l| l.into_owned()), h.finalize().to_vec()))
}

/// Whether every entry of `delta` is in the copy made of the `old` layers as it is (`Sig`). `uv sync` with
/// nothing to install still rewrites an empty `.venv/.lock`, which otherwise makes every step after a
/// `COPY --from` of the venv run again, where `docker build` finds them cached.
fn redundant(delta: &Path, old: &[PathBuf]) -> Result<bool> {
    let mut want: HashMap<String, Sig> = HashMap::new();
    for e in tar::Archive::new(open_layer(delta)?).entries()? {
        let mut e = e?;
        let p = norm_path(&e.path()?);
        if p.rsplit('/').next().is_some_and(|n| n.starts_with(".wh.")) || want.len() >= 10_000 {
            return Ok(false);
        }
        want.insert(p, sig(&mut e)?);
    }
    for l in old.iter().rev() {
        if want.is_empty() {
            break;
        }
        for e in tar::Archive::new(open_layer(l)?).entries()? {
            let mut e = e?;
            let p = norm_path(&e.path()?);
            let (dir, name) = p.rsplit_once('/').unwrap_or(("", p.as_str()));
            match name.strip_prefix(".wh.") {
                // Hidden in the copy: an entry the delta has there is new to it.
                Some(".wh..opq") => if want.keys().any(|w| is_under(w, dir) && w != dir) { return Ok(false) },
                Some(n) => if want.keys().any(|w| is_under(w, &if dir.is_empty() { n.to_string() } else { format!("{dir}/{n}") })) { return Ok(false) },
                None => if let Some(w) = want.remove(&p) && sig(&mut e)? != w { return Ok(false) },
            }
        }
    }
    Ok(want.is_empty())
}

struct Tr(Instant, u128, usize);
impl Drop for Tr {
    fn drop(&mut self) {
        if std::env::var_os("IMGBUILD_TRACE").is_some() {
            eprintln!("  trace layer {}: subset {}ms, total {}ms", self.2, self.1, self.0.elapsed().as_millis());
        }
    }
}

fn extract(dir: &Path, layers: &[PathBuf], prefixes: &[String]) -> Result<(HashMap<String, Meta>, Files)> {
    let tars = dir.with_extension("tars");
    let _ = fs::remove_dir_all(&tars);
    fs::create_dir_all(&tars)?;
    let mut meta: HashMap<String, Meta> = HashMap::new();
    let mut files = Files::default();
    let keep = |rel: &str| {
        prefixes.is_empty()
            || prefixes.iter().any(|p| p.is_empty() || rel == p || rel.starts_with(&format!("{p}/")) || p.starts_with(&format!("{rel}/")))
    };
    // A layer's subset for some prefixes is kept (a few libraries out of a base image, nothing out of a
    // node_modules layer): the next extract skips decompressing the whole layer.
    let subsets = node_cache("subsets").filter(|_| !prefixes.is_empty() && !prefixes.iter().any(String::is_empty));
    for (i, layer) in layers.iter().enumerate() {
        let t_layer = Instant::now();
        let cached = subsets.as_ref().map(|d| d.join(h(&["links", &layer.file_name().unwrap_or_default().to_string_lossy(), &prefixes.join("\0")])));
        let src = match &cached {
            _ if plain_of(layer).exists() => plain_of(layer),
            Some(c) if c.exists() => {
                touch(c);
                c.clone()
            }
            _ => {
                let t = tars.join(i.to_string());
                layer_subset(layer, prefixes, &t)?;
                if let Some(c) = &cached {
                    let _ = fs::create_dir_all(c.parent().unwrap()).and_then(|_| fs::hard_link(&t, c).or_else(|_| fs::copy(&t, c).map(|_| ())));
                }
                t
            }
        };
        let src = Arc::new(src);
        let t_sub = t_layer.elapsed().as_millis();
        let _tr = Tr(t_layer, t_sub, i);
        let mut ar = tar::Archive::new(File::open(src.as_ref())?);
        let mut skipped: HashMap<String, Node> = HashMap::new();
        for e in ar.entries_with_seek()? {
            let e = e?;
            let raw = e.path()?.to_string_lossy().to_string();
            let rel: Vec<&str> = raw.split('/').filter(|c| !c.is_empty() && *c != ".").collect();
            if rel.is_empty() || rel.contains(&"..") {
                continue;
            }
            let (name, parent) = (rel[rel.len() - 1], rel[..rel.len() - 1].join("/"));
            if !name.starts_with(".wh.") && !e.header().entry_type().is_symlink() && !keep(&rel.join("/")) {
                if matches!(e.header().entry_type(), tar::EntryType::Regular | tar::EntryType::Continuous) {
                    let m = e.header().mode()?;
                    skipped.insert(rel.join("/"), Node::File(src.clone(), e.raw_file_position(), e.size(), m & 0o111 != 0));
                }
                continue;
            }
            let p = files.resolve(&parent, true)?;
            if name == ".wh..wh..opq" {
                let kids: Vec<String> = files.names(&p).cloned().collect();
                for k in kids {
                    files.remove_tree(&if p.is_empty() { k } else { format!("{p}/{k}") });
                }
                if p.is_empty() {
                    meta.clear();
                } else {
                    meta.retain(|k, _| !k.starts_with(&format!("{p}/")));
                }
                continue;
            }
            if let Some(n) = name.strip_prefix(".wh.") {
                let t = if p.is_empty() { n.to_string() } else { format!("{p}/{n}") };
                files.remove_tree(&t);
                meta.retain(|k, _| !is_under(k, &t));
                continue;
            }
            let t = if p.is_empty() { name.to_string() } else { format!("{p}/{name}") };
            let hd = e.header();
            let m = Meta { uid: hd.uid()?, gid: hd.gid()?, mode: hd.mode()?, mtime: hd.mtime()? };
            let node = match hd.entry_type() {
                tar::EntryType::Directory => {
                    if matches!(files.at.get(&t), Some(Node::Dir)) {
                        meta.insert(t, m);
                        continue;
                    }
                    Node::Dir
                }
                tar::EntryType::Regular | tar::EntryType::Continuous => Node::File(src.clone(), e.raw_file_position(), e.size(), m.mode & 0o111 != 0),
                tar::EntryType::Symlink => Node::Link(e.link_name()?.context("symlink without target")?.into_owned()),
                tar::EntryType::Link => {
                    let target = e.link_name()?.context("hardlink without target")?.to_string_lossy().to_string();
                    match files.at.get(&files.resolve(&target, false)?).or_else(|| skipped.get(&norm_path(Path::new(&target)))) {
                        Some(n @ (Node::File(..) | Node::Link(_))) => n.clone(),
                        _ => continue,
                    }
                }
                _ => continue,
            };
            files.remove_tree(&t);
            files.put(&t, node);
            meta.insert(t, m);
        }
    }
    Ok((meta, files))
}

/// COPY semantics: a directory source copies its contents, several sources (or a glob, or a trailing `/`)
/// copy into the destination directory.
#[allow(clippy::too_many_arguments)]
fn copy_entries(root: &Path, files: Option<&Files>, meta: Option<&HashMap<String, Meta>>, ignore: Option<&Ignore>, sources: &[String], dest: &str,
    chown: Option<(u64, u64)>, chmod: Option<u32>) -> Result<Vec<Entry>> {
    let into_dir = dest.ends_with('/') || sources.len() > 1 || sources.iter().any(|s| is_glob(s));
    let dest = dest.trim_matches('/').to_string();
    let mut out = vec![];
    let meta_of = |rel: &str, st: &Stat| -> Meta {
        let mut x = match meta {
            Some(map) => map.get(rel).copied().unwrap_or(Meta { mode: if st.dir { 0o755 } else { 0o644 }, ..Default::default() }),
            None => Meta { uid: 0, gid: 0, mode: st.mode & 0o7777, mtime: st.mtime },
        };
        if let Some((u, g)) = chown {
            (x.uid, x.gid) = (u, g);
        }
        if let Some(c) = chmod {
            x.mode = c;
        }
        x
    };
    let skip = |rel: &str| ignore.is_some_and(|i| i.excluded(rel));
    for src in sources {
        let matches = glob(root, files, dockerfile::clean_src(src))?;
        ensure!(!matches.is_empty(), "COPY source {src} not found");
        for rel in matches {
            if skip(&rel) {
                continue;
            }
            // A source that is a symlink is followed, as BuildKit does: its target is copied under its name.
            let src = match stat(root, files, &rel)?.kind {
                Kind::Link(_) => match files {
                    Some(f) => f.resolve(&rel, true)?,
                    None => resolve(root, &rel, true)?,
                },
                _ => rel.clone(),
            };
            let st = stat(root, files, &src).with_context(|| format!("COPY source {rel}"))?;
            if st.dir {
                // The destination directory itself is not the source's: make_layer keeps it or creates it.
                walk(root, files, &src, &dest, &mut out, &meta_of, &skip, ignore.is_some_and(|i| i.has_exceptions()))?;
            } else {
                let name = Path::new(&rel).file_name().context("source name")?.to_string_lossy().to_string();
                let path = if into_dir { format!("{dest}/{name}").trim_start_matches('/').to_string() } else { dest.clone() };
                let meta = meta_of(&src, &st);
                let kind = match st.kind {
                    Kind::Slice(p, off, len, x) if src != rel => Kind::Slice(Arc::new(p.as_ref().clone()), off, len, x),
                    k => k,
                };
                out.push(Entry { path, meta, kind });
            }
        }
    }
    Ok(out)
}

struct Stat {
    dir: bool,
    /// permission bits on disk (only meaningful for a context file)
    mode: u32,
    /// for a file or symlink, what a COPY of it writes
    kind: Kind,
    /// neither a directory, file nor symlink (a socket, a device)
    other: bool,
    /// a context file's modification time, which BuildKit's COPY keeps (cargo, make and the like compare it)
    mtime: u64,
}

fn stat(root: &Path, files: Option<&Files>, rel: &str) -> Result<Stat> {
    if let Some(f) = files {
        if rel.is_empty() {
            return Ok(Stat { dir: true, mode: 0o755, kind: Kind::Dir, other: false, mtime: 0 });
        }
        let n = f.at.get(rel).with_context(|| format!("{rel} not found"))?;
        return Ok(match n {
            Node::Dir => Stat { dir: true, mode: 0o755, kind: Kind::Dir, other: false, mtime: 0 },
            Node::File(p, off, len, x) => Stat { dir: false, mode: if *x { 0o755 } else { 0o644 }, kind: Kind::Slice(p.clone(), *off, *len, *x), other: false, mtime: 0 },
            Node::Link(t) => Stat { dir: false, mode: 0o777, kind: Kind::Link(t.clone()), other: false, mtime: 0 },
        });
    }
    let disk = root.join(rel);
    let m = fs::symlink_metadata(&disk)?;
    let kind = if m.file_type().is_symlink() { Kind::Link(fs::read_link(&disk)?) } else if m.is_dir() { Kind::Dir } else { Kind::File(disk) };
    let mtime = std::os::unix::fs::MetadataExt::mtime(&m).max(0) as u64;
    Ok(Stat { dir: m.is_dir(), mode: m.permissions().mode(), kind, other: !(m.is_dir() || m.is_file() || m.file_type().is_symlink()), mtime })
}

#[allow(clippy::too_many_arguments)]
fn walk(root: &Path, files: Option<&Files>, rel: &str, dest: &str, out: &mut Vec<Entry>, meta_of: &dyn Fn(&str, &Stat) -> Meta,
    skip: &dyn Fn(&str) -> bool, descend_excluded: bool) -> Result<()> {
    let mut names: Vec<String> = match files {
        Some(f) => f.names(rel).cloned().collect(),
        None => list_dir(&root.join(rel))?,
    };
    names.sort();
    for n in names {
        let r = if rel.is_empty() { n.clone() } else { format!("{rel}/{n}") };
        let d = if dest.is_empty() { n.clone() } else { format!("{dest}/{n}") };
        let st = stat(root, files, &r)?;
        let excluded = skip(&r);
        if st.dir {
            if !excluded {
                out.push(Entry { path: d.clone(), kind: Kind::Dir, meta: meta_of(&r, &st) });
            }
            if !excluded || descend_excluded {
                walk(root, files, &r, &d, out, meta_of, skip, descend_excluded)?;
            }
        } else if !excluded && !st.other {
            out.push(Entry { path: d, meta: meta_of(&r, &st), kind: st.kind });
        }
    }
    Ok(())
}

type StatKey = (u64, u64, i128, i128, u32);

fn stat_key(m: &fs::Metadata) -> StatKey {
    use std::os::unix::fs::MetadataExt;
    (m.len(), m.ino(), m.mtime() as i128 * 1_000_000_000 + m.mtime_nsec() as i128, m.ctime() as i128 * 1_000_000_000 + m.ctime_nsec() as i128, m.mode())
}

/// Context file digests and directory listings by absolute path, checked against size, inode, mtime,
/// ctime and mode: a hot build stats the context instead of reading it, as git and BuildKit do. Kept
/// across builds in the node cache. A directory's mtime changes when an entry is added, removed or renamed.
#[derive(Default)]
struct Ctx {
    sums: HashMap<PathBuf, (StatKey, [u8; 32])>,
    dirs: HashMap<PathBuf, (StatKey, Vec<String>)>,
    dirty: bool,
}

static CTX: std::sync::LazyLock<Mutex<Ctx>> = std::sync::LazyLock::new(|| {
    let mut c = Ctx::default();
    for l in node_cache("context-stat").and_then(|f| fs::read_to_string(f).ok()).unwrap_or_default().lines() {
        let f: Vec<&str> = l.splitn(8, ' ').collect();
        let [t, v, a, b, cc, e, g, p] = f[..] else { continue };
        let (Ok(a), Ok(b), Ok(cc), Ok(e), Ok(g)) = (a.parse(), b.parse(), cc.parse(), e.parse(), g.parse()) else { continue };
        let k = (a, b, cc, e, g);
        match t {
            "f" => {
                if let Some(d) = hex::decode(v).ok().and_then(|d| <[u8; 32]>::try_from(d).ok()) {
                    c.sums.insert(PathBuf::from(p), (k, d));
                }
            }
            "d" => {
                c.dirs.insert(PathBuf::from(p), (k, v.split('/').filter(|n| !n.is_empty()).map(String::from).collect()));
            }
            _ => {}
        }
    }
    Mutex::new(c)
});

/// Only what last changed clearly in the past is remembered: something changed again within the clock's
/// resolution after being read could keep its stat ("racily clean", as git calls it). Linux ticks file
/// times at ~4 ms, and mtime can be set back, so the ctime decides too.
fn settled(m: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
    now - m.ctime() >= 2 && now - m.mtime() >= 2
}

fn context_digest(p: &Path) -> Result<[u8; 32]> {
    let m = fs::symlink_metadata(p)?;
    if m.file_type().is_symlink() {
        return file_digest(p);
    }
    let p = std::path::absolute(p)?;
    let k = stat_key(&m);
    if let Some((k0, d)) = CTX.lock().unwrap().sums.get(&p)
        && *k0 == k {
            return Ok(*d);
        }
    let d = file_digest(&p)?;
    if settled(&m) {
        let mut g = CTX.lock().unwrap();
        g.sums.insert(p, (k, d));
        g.dirty = true;
    }
    Ok(d)
}

/// The names in context directory `d` (readdir costs ~75 µs a directory on macOS).
fn list_dir(d: &Path) -> Result<Vec<String>> {
    let m = fs::symlink_metadata(d)?;
    let d = std::path::absolute(d)?;
    let k = stat_key(&m);
    if let Some((k0, v)) = CTX.lock().unwrap().dirs.get(&d)
        && *k0 == k {
            return Ok(v.clone());
        }
    let v = fs::read_dir(&d)?.map(|e| e.map(|e| e.file_name().to_string_lossy().to_string())).collect::<io::Result<Vec<_>>>()?;
    if settled(&m) && !v.iter().any(|n| n.contains(['\n', ' '])) {
        let mut g = CTX.lock().unwrap();
        g.dirs.insert(d, (k, v.clone()));
        g.dirty = true;
    }
    Ok(v)
}

/// Writes `CTX` back when this run added to it.
fn save_ctx() {
    let Some(f) = node_cache("context-stat") else { return };
    let mut g = CTX.lock().unwrap();
    if !g.dirty {
        return;
    }
    // ponytail: entries of deleted paths stay; the file is started over past 1M entries.
    if g.sums.len() + g.dirs.len() > 1_000_000 {
        g.sums.clear();
        g.dirs.clear();
    }
    let mut out = String::new();
    let line = |out: &mut String, t: &str, v: &str, (a, b, c, e, h): &StatKey, p: &Path| {
        if let Some(p) = p.to_str().filter(|p| !p.contains('\n')) {
            out.push_str(&format!("{t} {v} {a} {b} {c} {e} {h} {p}\n"));
        }
    };
    for (p, (k, d)) in &g.sums {
        line(&mut out, "f", &hex::encode(d), k, p);
    }
    for (p, (k, names)) in &g.dirs {
        line(&mut out, "d", &format!("/{}", names.join("/")), k, p);
    }
    let tmp = f.with_extension(format!("tmp{}", uniq()));
    if fs::create_dir_all(f.parent().unwrap()).and_then(|_| fs::write(&tmp, out)).and_then(|_| fs::rename(&tmp, &f)).is_err() {
        let _ = fs::remove_file(&tmp);
    }
    g.dirty = false;
}

/// Content key of context sources: names, modes and file contents, hashed in parallel.
fn context_key(root: &Path, ignore: &Ignore, sources: &[String]) -> Result<String> {
    entries_key(&copy_entries(root, None, None, Some(ignore), sources, "/k/", None, None)?)
}

/// What a layer of `entries` contains, without timestamps: equal keys mean interchangeable layers.
fn entries_key(entries: &[Entry]) -> Result<String> {
    let files: Vec<&Entry> = entries.iter().collect();
    let n = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    let mut sums = vec![[0u8; 32]; files.len()];
    let chunk = files.len().div_ceil(n).max(1);
    std::thread::scope(|sc| -> Result<()> {
        let hs: Vec<_> = files.chunks(chunk).zip(sums.chunks_mut(chunk)).map(|(f, o)| {
            sc.spawn(move || -> Result<()> {
                for (i, e) in f.iter().enumerate() {
                    o[i] = match &e.kind {
                        Kind::File(p) => context_digest(p)?,
                        Kind::Slice(p, off, len, x) => {
                            let mut h = Sha256::new();
                            h.update(if *x { b"x" } else { b"f" });
                            let mut r = SliceReader { f: Arc::new(File::open(p.as_ref())?), off: *off, end: off + len };
                            let mut buf = vec![0u8; 1 << 16];
                            loop {
                                let n = r.read(&mut buf)?;
                                if n == 0 {
                                    break;
                                }
                                h.update(&buf[..n]);
                            }
                            h.finalize().into()
                        }
                        Kind::Link(t) => Sha256::digest(t.to_string_lossy().as_bytes()).into(),
                        Kind::Dir => [0; 32],
                    };
                }
                Ok(())
            })
        }).collect();
        for h in hs {
            h.join().map_err(|_| anyhow::anyhow!("hash thread panicked"))??;
        }
        Ok(())
    })?;
    let mut d = Sha256::new();
    for (e, s) in files.iter().zip(&sums) {
        d.update(format!("{}\0{:o}\0{}\0{}\0", e.path, e.meta.mode, e.meta.uid, e.meta.gid));
        d.update(s);
    }
    Ok(hex::encode(d.finalize()))
}

#[derive(Clone)]
struct Blob {
    digest: String,
    diff_id: String,
    size: u64,
    media_type: String,
    host: String,
    repo: String,
    local: Option<PathBuf>,
    /// directories in the layer, when known without reading it
    dirs: Option<Dirs>,
}

type Dirs = Arc<BTreeMap<String, Meta>>;

fn dirs_of(entries: &[Entry]) -> Dirs {
    Arc::new(entries.iter().filter(|e| matches!(e.kind, Kind::Dir)).map(|e| (e.path.clone(), e.meta)).collect())
}

fn dirs_json(d: &Dirs) -> String {
    serde_json::to_string(&d.iter().map(|(k, m)| (k.clone(), [m.uid, m.gid, m.mode as u64, m.mtime])).collect::<BTreeMap<_, _>>()).unwrap()
}

fn dirs_from(layer: &Value) -> Option<Dirs> {
    let m: BTreeMap<String, [u64; 4]> = serde_json::from_str(layer["annotations"]["imgbuild.dirs"].as_str()?).ok()?;
    Some(Arc::new(m.into_iter().map(|(k, [uid, gid, mode, mtime])| (k, Meta { uid, gid, mode: mode as u32, mtime })).collect()))
}

/// Directories of a layer file, read from its tar headers.
fn scan_dirs(path: &Path) -> Result<BTreeMap<String, Meta>> {
    let mut out = BTreeMap::new();
    for e in tar::Archive::new(open_layer(path)?).entries()? {
        let e = e?;
        let h = e.header();
        if h.entry_type().is_dir() {
            let p = e.path()?.to_string_lossy().split('/').filter(|c| !c.is_empty() && *c != ".").collect::<Vec<_>>().join("/");
            if !p.is_empty() {
                out.insert(p, Meta { uid: h.uid()?, gid: h.gid()?, mode: h.mode()?, mtime: h.mtime()? });
            }
        }
    }
    Ok(out)
}

#[derive(Clone)]
struct State {
    layers: Vec<Blob>,
    cfg: Value,
    key: String,
}

enum Src {
    Context,
    Stage(usize),
    Image(String),
}

enum Op {
    Config,
    /// `chown`: user and group as written (numeric ids or names); an empty group means the uid
    Copy { src: Src, sources: Vec<String>, dest: String, chown: Option<(String, String)>, chmod: Option<u32> },
    Run { argv: Vec<String>, env: Vec<String>, mounts: Vec<(String, String)>, secrets: Vec<(String, String)>, network: Option<String> },
}

struct Step {
    /// key of the state after this step from the inputs alone (a `COPY --from` stands for its source's
    /// input key, not the copied content); published next to the content key so an unchanged build is
    /// found in one lookup
    ikey: String,
    text: String,
    /// content key of what a context COPY reads
    extra: String,
    cfg: Value,
    op: Op,
}

enum Base {
    Scratch,
    Image(State),
    Stage(usize),
}

struct Plan {
    name: String,
    platform: String,
    base: Base,
    /// config before the first step
    init_cfg: Value,
    steps: Vec<Step>,
    cfg: Value,
    ikey: String,
}

type Memo<T> = Mutex<HashMap<String, Arc<OnceCell<T>>>>;

async fn lock_run(dir: &Path, key: &str, label: &str) -> Result<File> {
    fs::create_dir_all(dir)?;
    // Waiters must lock the same inode. Older builders unlink their unprefixed markers.
    let file = File::options().write(true).create(true).truncate(false).open(dir.join(format!("run-{key}")))?;
    let mut waiting = false;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(fs::TryLockError::WouldBlock) => {
                if !waiting {
                    eprintln!("{label}: another build on this machine runs the same step, waiting for it");
                    waiting = true;
                }
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
            Err(e) => return Err(e.into()),
        }
    }
}

fn cell<T>(m: &Memo<T>, k: String) -> Arc<OnceCell<T>> {
    m.lock().unwrap().entry(k).or_default().clone()
}

struct B {
    o: Opts,
    globals: Vec<Instr>,
    stages: Vec<Stage>,
    ignore: Ignore,
    args: BTreeMap<String, String>,
    host: String,
    cache: Ref,
    regs: Mutex<HashMap<String, Reg>>,
    plans: Memo<Arc<Plan>>,
    states: Memo<std::result::Result<State, String>>,
    images: Memo<State>,
    tops: Memo<(Bytes, String, String)>,
    published: Memo<String>,
    rootfs: Memo<Arc<Rootfs>>,
    uploads: Memo<std::result::Result<(), String>>,
    live: Option<crate::live::Live>,
    early_run: Option<Arc<tokio::sync::Semaphore>>,
    dest: Ref,
    target: usize,
    dirs: Memo<Dirs>,
    blobs: Memo<PathBuf>,
    docker: OnceCell<()>,
    binfmt: Memo<()>,
    t0: Instant,
}

fn local_blob(l: crate::layer::Layer, dirs: Dirs) -> Blob {
    Blob { digest: l.digest, diff_id: l.diff_id, size: l.size, media_type: "application/vnd.oci.image.layer.v1.tar+zstd".into(), host: String::new(), repo: String::new(), local: Some(l.path), dirs: Some(dirs) }
}

fn src_id(s: &Src) -> String {
    match s {
        Src::Stage(j) => format!("stage {j}"),
        Src::Image(r) => format!("image {r}"),
        Src::Context => String::new(),
    }
}

/// The literal leading directories of a source path, before any glob.
fn prefix(s: &str) -> String {
    s.split('/').filter(|c| !c.is_empty() && *c != ".").take_while(|c| !is_glob(c)).collect::<Vec<_>>().join("/")
}

fn with_diff_ids(cfg: &Value, layers: &[Blob]) -> Value {
    let mut c = cfg.clone();
    c["rootfs"] = json!({"type": "layers", "diff_ids": layers.iter().map(|l| l.diff_id.clone()).collect::<Vec<_>>()});
    c
}

fn sh(cmd: &mut Command) -> Result<String> {
    let out = cmd.stderr(Stdio::inherit()).output().with_context(|| format!("{cmd:?}"))?;
    ensure!(out.status.success(), "{cmd:?} failed: {}", out.status);
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

impl B {
    fn log(&self, msg: impl AsRef<str>) {
        eprintln!("[{:>6.2}s] {}", self.t0.elapsed().as_secs_f64(), msg.as_ref());
    }

    fn reg(&self, host: &str) -> Reg {
        self.regs.lock().unwrap().entry(host.to_string()).or_insert_with(|| Reg::new(host)).clone()
    }

    fn platform_vars(&self, tp: &str) -> BTreeMap<String, String> {
        let (tos, tarch) = tp.split_once('/').unwrap_or(("linux", tp));
        let (bos, barch) = self.host.split_once('/').unwrap();
        [("TARGETPLATFORM", tp), ("TARGETOS", tos), ("TARGETARCH", tarch), ("TARGETVARIANT", ""),
         ("BUILDPLATFORM", self.host.as_str()), ("BUILDOS", bos), ("BUILDARCH", barch), ("BUILDVARIANT", "")]
            .into_iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    fn global_vars(&self, tp: &str) -> BTreeMap<String, String> {
        let mut v = self.platform_vars(tp);
        for ins in &self.globals {
            for w in dockerfile::words(&ins.rest) {
                let (k, d) = w.split_once('=').map(|(k, d)| (k.to_string(), Some(d.to_string()))).unwrap_or((w.clone(), None));
                let val = self.args.get(&k).cloned().or(d.map(|d| expand(&d, &v))).unwrap_or_default();
                v.insert(k, val);
            }
        }
        v
    }

    /// A registry image for one platform: its layers stay where they are until something needs them.
    async fn image(self: &Arc<Self>, r: &str, platform: &str) -> Result<State> {
        let c = cell(&self.images, format!("{r}|{platform}"));
        c.get_or_try_init(|| async {
            let rf = Ref::parse(r)?;
            let reg = self.reg(&rf.host);
            let top = cell(&self.tops, r.to_string());
            let _s = crate::prof::span("image", || format!("{r} {platform}"));
            let (body, ct, mut digest) = top.get_or_try_init(|| async {
                // An image another job is still building (e.g. the assets image of the other CPU's job): the
                // stages that do not need it go on meanwhile.
                let wait = std::env::var("IMGBUILD_WAIT_FOR_IMAGES").ok().and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
                let t = Instant::now();
                if wait > 0 && !reg.manifest_exists(&rf.repo, &rf.reference).await? {
                    eprintln!("waiting up to {wait}s for {r}");
                    while !reg.manifest_exists(&rf.repo, &rf.reference).await? {
                        ensure!(t.elapsed().as_secs() < wait, "{r} did not appear within {wait}s");
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    }
                    eprintln!("{r} appeared after {:.0}s", t.elapsed().as_secs_f64());
                }
                reg.manifest(&rf.repo, &rf.reference).await
            }).await?.clone();
            let mut m: Value = serde_json::from_slice(&body)?;
            if ct.contains("index") || ct.contains("manifest.list") || m["manifests"].is_array() {
                let (os, arch) = platform.split_once('/').context("platform os/arch")?;
                digest = m["manifests"].as_array().unwrap().iter()
                    .find(|e| e["platform"]["os"] == os && e["platform"]["architecture"] == arch)
                    .with_context(|| format!("{r} has no {platform} image"))?["digest"].as_str().context("digest")?.to_string();
                m = serde_json::from_slice(&reg.manifest(&rf.repo, &digest).await?.0)?;
            }
            let cfg: Value = serde_json::from_slice(&reg.get_blob(&rf.repo, m["config"]["digest"].as_str().context("config")?).await?)?;
            let diff_ids = cfg["rootfs"]["diff_ids"].as_array().context("diff_ids")?;
            let layers = m["layers"].as_array().context("layers")?.iter().zip(diff_ids).map(|(l, d)| Blob {
                digest: l["digest"].as_str().unwrap_or_default().into(),
                diff_id: d.as_str().unwrap_or_default().into(),
                size: l["size"].as_u64().unwrap_or(0),
                media_type: oci_layer_type(l["mediaType"].as_str().unwrap_or_default()),
                host: rf.host.clone(),
                repo: rf.repo.clone(),
                local: None,
                dirs: dirs_from(l),
            }).collect();
            Ok::<_, anyhow::Error>(State { layers, cfg, key: h(&[FORMAT, "image", &digest]) })
        }).await.cloned()
    }

    fn plan<'a>(self: &'a Arc<Self>, idx: usize, tp: &'a str) -> BoxFuture<'a, Result<Arc<Plan>>> {
        async move {
            let c = cell(&self.plans, format!("{idx}|{tp}"));
            c.get_or_try_init(|| self.make_plan(idx, tp)).await.cloned()
        }.boxed()
    }

    async fn make_plan(self: &Arc<Self>, idx: usize, tp: &str) -> Result<Arc<Plan>> {
        let st = &self.stages[idx];
        let name = st.name.clone().unwrap_or_else(|| format!("stage {idx}"));
        let gvars = self.global_vars(tp);
        let sp = st.platform.as_deref().map(|p| expand(p, &gvars)).unwrap_or_else(|| tp.to_string());
        let from = expand(&st.from, &gvars);
        let (base, mut cfg, mut ikey) = if from == "scratch" {
            let (os, arch) = sp.split_once('/').unwrap_or(("linux", &sp));
            (Base::Scratch, json!({"architecture": arch, "os": os, "config": {}, "rootfs": {"type": "layers", "diff_ids": []}, "history": []}), h(&[FORMAT, "scratch", &sp]))
        } else if let Some(j) = dockerfile::find_stage(&self.stages, &from).filter(|&j| j < idx) {
            let p = self.plan(j, tp).await?;
            (Base::Stage(j), p.cfg.clone(), p.ikey.clone())
        } else {
            let s = self.image(&from, &sp).await?;
            let (c, k) = (s.cfg.clone(), s.key.clone());
            (Base::Image(s), c, k)
        };
        if !cfg["config"].is_object() {
            cfg["config"] = json!({});
        }
        if !cfg["history"].is_array() {
            cfg["history"] = json!([]);
        }
        let init_cfg = cfg.clone();
        let created = crate::rfc3339(self.o.epoch);
        let pvars = self.platform_vars(tp);
        let mut args: BTreeMap<String, String> = BTreeMap::new();
        let mut shell = vec!["/bin/sh".to_string(), "-c".to_string()];
        let mut cmd_set = false;
        let mut steps = vec![];
        for ins in &st.instrs {
            let env: Vec<String> = cfg["config"]["Env"].as_array().map(|a| a.iter().filter_map(|e| e.as_str().map(String::from)).collect()).unwrap_or_default();
            let mut vars = args.clone();
            for e in &env {
                if let Some((k, v)) = e.split_once('=') {
                    vars.insert(k.into(), v.into());
                }
            }
            let x = |s: &str| expand(s, &vars);
            let o = &mut cfg["config"];
            let mut extra = String::new();
            let mut text = format!("{} {}", ins.cmd, ins.rest);
            let op = match ins.cmd.as_str() {
                "ARG" => {
                    let mut parts = vec![];
                    for w in dockerfile::words(&ins.rest) {
                        let (k, d) = w.split_once('=').map(|(k, d)| (k.to_string(), Some(d.to_string()))).unwrap_or((w.clone(), None));
                        let v = self.args.get(&k).cloned()
                            .or_else(|| pvars.get(&k).filter(|_| d.is_none()).cloned())
                            .or_else(|| d.is_none().then(|| gvars.get(&k).cloned()).flatten())
                            .or(d.map(|d| x(&d)))
                            .unwrap_or_default();
                        parts.push(format!("{k}={v}"));
                        args.insert(k, v);
                    }
                    text = format!("ARG {}", parts.join(" "));
                    Op::Config
                }
                "ENV" => {
                    let w = dockerfile::words(&ins.rest);
                    let pairs: Vec<(String, String)> = if w.iter().all(|p| p.contains('=')) {
                        w.iter().map(|p| { let (k, v) = p.split_once('=').unwrap(); (k.to_string(), x(v)) }).collect()
                    } else {
                        let (k, v) = ins.rest.split_once(char::is_whitespace).context("ENV needs a value")?;
                        vec![(k.to_string(), x(v.trim()))]
                    };
                    let mut env = env;
                    for (k, v) in &pairs {
                        env.retain(|e| e.split('=').next() != Some(k));
                        env.push(format!("{k}={v}"));
                    }
                    o["Env"] = json!(env);
                    text = format!("ENV {}", pairs.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(" "));
                    Op::Config
                }
                "WORKDIR" => {
                    let w = dockerfile::join_abs(o["WorkingDir"].as_str().filter(|s| !s.is_empty()), &x(&ins.rest));
                    o["WorkingDir"] = json!(w);
                    text = format!("WORKDIR {w}");
                    Op::Config
                }
                "USER" => {
                    o["User"] = json!(x(&ins.rest));
                    Op::Config
                }
                "LABEL" => {
                    for w in dockerfile::words(&x(&ins.rest)) {
                        let (k, v) = w.split_once('=').with_context(|| format!("LABEL {w}"))?;
                        o["Labels"][k] = json!(v);
                    }
                    Op::Config
                }
                "EXPOSE" => {
                    for p in dockerfile::words(&x(&ins.rest)) {
                        let p = if p.contains('/') { p } else { format!("{p}/tcp") };
                        o["ExposedPorts"][p] = json!({});
                    }
                    Op::Config
                }
                "CMD" | "ENTRYPOINT" => {
                    let v = dockerfile::json_list(&ins.rest).unwrap_or_else(|| [shell.clone(), vec![ins.rest.clone()]].concat());
                    if ins.cmd == "CMD" {
                        o["Cmd"] = json!(v);
                        cmd_set = true;
                    } else {
                        o["Entrypoint"] = json!(v);
                        if !cmd_set {
                            o["Cmd"] = Value::Null;
                        }
                    }
                    Op::Config
                }
                "SHELL" => {
                    shell = dockerfile::json_list(&ins.rest).context("SHELL must be a JSON list")?;
                    o["Shell"] = json!(shell);
                    Op::Config
                }
                "STOPSIGNAL" => {
                    o["StopSignal"] = json!(x(&ins.rest));
                    Op::Config
                }
                "VOLUME" => {
                    for v in dockerfile::json_list(&ins.rest).unwrap_or_else(|| dockerfile::words(&x(&ins.rest))) {
                        o["Volumes"][v] = json!({});
                    }
                    Op::Config
                }
                "HEALTHCHECK" if ins.rest.trim().eq_ignore_ascii_case("NONE") => {
                    o["Healthcheck"] = json!({"Test": ["NONE"]});
                    Op::Config
                }
                "MAINTAINER" => Op::Config,
                "COPY" | "ADD" => {
                    let (flags, mut list) = match dockerfile::json_list(&ins.rest) {
                        Some(l) => (vec![], l),
                        None => dockerfile::copy_flags(&ins.rest),
                    };
                    list = list.iter().map(|a| x(a)).collect();
                    let (mut from, mut chown, mut chmod) = (None, None, None);
                    for (k, v) in flags {
                        match k.as_str() {
                            "from" => from = Some(x(&v)),
                            "chown" => {
                                let v = x(&v);
                                let numeric = |s: &str| s.parse::<u64>().is_ok();
                                let (u, g) = v.split_once(':').unwrap_or((&v, if numeric(&v) { &v } else { "" }));
                                chown = Some((u.to_string(), g.to_string()));
                            }
                            "chmod" => chmod = Some(u32::from_str_radix(&v, 8).with_context(|| format!("--chmod={v}"))?),
                            "link" => {}
                            _ => bail!("{} --{k} is not supported", ins.cmd),
                        }
                    }
                    ensure!(list.len() >= 2, "{} needs a source and a destination", ins.cmd);
                    let raw_dest = list.pop().unwrap();
                    // As BuildKit: `.`, `x/` and `x/.` name a directory; the path is cleaned (`/app/.` is `/app`).
                    let mut dest = format!("/{}", norm_path(Path::new(&dockerfile::join_abs(o["WorkingDir"].as_str().filter(|s| !s.is_empty()), &raw_dest))));
                    if (raw_dest == "." || raw_dest.ends_with('/') || raw_dest.ends_with("/.")) && !dest.ends_with('/') {
                        dest.push('/');
                    }
                    let src = match &from {
                        None => Src::Context,
                        Some(f) => match dockerfile::find_stage(&self.stages, f).filter(|&j| j < idx) {
                            Some(j) => Src::Stage(j),
                            None => Src::Image(f.clone()),
                        },
                    };
                    if ins.cmd == "ADD" {
                        for s in &list {
                            ensure!(!s.contains("://"), "ADD {s}: URLs are not supported");
                            let archive = [".tar", ".tar.gz", ".tgz", ".tar.xz", ".tar.bz2", ".tar.zst"].iter().any(|e| s.ends_with(e));
                            ensure!(!(archive && matches!(src, Src::Context)), "ADD {s}: archive extraction is not supported");
                        }
                    }
                    if let Src::Context = src {
                        let (root, ign, srcs) = (self.o.context.clone(), &self.ignore, list.clone());
                        let t = Instant::now();
                        extra = tokio::task::block_in_place(|| context_key(&root, ign, &srcs))?;
                        if std::env::var_os("IMGBUILD_TRACE").is_some() {
                            eprintln!("  trace context {srcs:?}: {:.1}ms", t.elapsed().as_secs_f64() * 1e3);
                        }
                    }
                    let flags = [from.as_ref().map(|f| format!("--from={f} ")), chown.as_ref().map(|(u, g)| if g.is_empty() { format!("--chown={u} ") } else { format!("--chown={u}:{g} ") }), chmod.map(|m| format!("--chmod={m:o} "))];
                    text = format!("{} {}{} {}", ins.cmd, flags.into_iter().flatten().collect::<String>(), list.join(" "), dest);
                    Op::Copy { src, sources: list, dest, chown, chmod }
                }
                "RUN" => {
                    let mut rest = ins.rest.trim_start();
                    let (mut mounts, mut secrets, mut network) = (vec![], vec![], None);
                    while let Some(r) = rest.strip_prefix("--") {
                        let (flag, tail) = r.split_once(char::is_whitespace).unwrap_or((r, ""));
                        rest = tail.trim_start();
                        match flag.split_once('=') {
                            Some(("mount", spec)) => {
                                let kv: BTreeMap<&str, &str> = spec.split(',').filter_map(|p| p.split_once('=')).collect();
                                if kv.get("type") == Some(&"secret") {
                                    // The value is in the environment as IMGBUILD_SECRET_<id>, and only in this RUN's container:
                                    // it is in no cache key, layer or image config.
                                    let (id, var) = (kv.get("id").context("secret mount needs an id")?, kv.get("env").with_context(|| format!("RUN --mount={spec}: only env= secrets are supported"))?);
                                    secrets.push((format!("IMGBUILD_SECRET_{id}"), var.to_string()));
                                    continue;
                                }
                                ensure!(kv.get("type") == Some(&"cache"), "RUN --mount={spec}: only type=cache and type=secret are supported");
                                let target = kv.get("target").or(kv.get("dst")).or(kv.get("destination")).context("cache mount needs a target")?;
                                let id = kv.get("id").copied().unwrap_or(target);
                                // One volume for every platform, as in BuildKit: what a cache holds that does not depend on
                                // the CPU (downloads, wheels built from source) is then made once, by the native run.
                                mounts.push((format!("imgbuild-cache-{}", &h(&[id])[..16]), target.to_string()));
                            }
                            Some(("network", n)) => network = Some(n.to_string()),
                            _ => bail!("RUN --{flag} is not supported"),
                        }
                    }
                    ensure!(!rest.starts_with("<<"), "RUN heredocs are not supported");
                    let argv = dockerfile::json_list(rest).unwrap_or_else(|| [shell.clone(), vec![rest.to_string()]].concat());
                    let names: Vec<&str> = env.iter().filter_map(|e| e.split('=').next()).collect();
                    let env = args.iter().filter(|(k, _)| !names.contains(&k.as_str())).map(|(k, v)| format!("{k}={v}")).collect();
                    Op::Run { argv, env, mounts, secrets, network }
                }
                other => bail!("{other} is not supported (stage {name})"),
            };
            cfg["history"].as_array_mut().unwrap().push(json!({"created": created, "created_by": text, "comment": "imgbuild", "empty_layer": matches!(op, Op::Config)}));
            let input = match &op {
                Op::Copy { src: Src::Stage(j), .. } => self.plan(*j, tp).await?.ikey.clone(),
                Op::Copy { src: Src::Image(r), .. } => self.image(r, tp).await?.key,
                _ => extra.clone(),
            };
            ikey = h(&["input", &ikey, &sp, &text, &input]);
            steps.push(Step { ikey: ikey.clone(), text, extra, cfg: cfg.clone(), op });
        }
        Ok(Arc::new(Plan { name, platform: sp, base, init_cfg, steps, cfg, ikey }))
    }

    async fn cache_get(&self, key: &str) -> Result<Option<State>> {
        if let Some(st) = local_step(key) {
            return Ok(Some(st));
        }
        let reg = self.reg(&self.cache.host);
        let mut sp = crate::prof::span("cache-lookup", || key[..12].to_string());
        let Some(body) = reg.manifest_opt(&self.cache.repo, &format!("c-{key}")).await? else {
            crate::prof::note(&mut sp, "miss");
            return Ok(None);
        };
        crate::prof::note(&mut sp, "hit");
        let m: Value = serde_json::from_slice(&body)?;
        let ls = m["layers"].as_array().context("layers")?;
        // The caller replaces the config with the step's own; only the diff_ids matter, annotated on each layer.
        let (cfg, diff_ids) = match ls.iter().map(|l| l["annotations"]["imgbuild.diff_id"].as_str().map(|d| json!(d))).collect::<Option<Vec<_>>>() {
            Some(d) => (Value::Null, d),
            None => {
                let cfg: Value = serde_json::from_slice(&reg.get_blob(&self.cache.repo, m["config"]["digest"].as_str().context("config")?).await?)?;
                let d = cfg["rootfs"]["diff_ids"].as_array().context("diff_ids")?.clone();
                (cfg, d)
            }
        };
        let layers = m["layers"].as_array().context("layers")?.iter().zip(&diff_ids).map(|(l, d)| Blob {
            digest: l["digest"].as_str().unwrap_or_default().into(),
            diff_id: d.as_str().unwrap_or_default().into(),
            size: l["size"].as_u64().unwrap_or(0),
            media_type: l["mediaType"].as_str().unwrap_or_default().into(),
            host: self.cache.host.clone(),
            repo: self.cache.repo.clone(),
            local: None,
            dirs: dirs_from(l),
        }).collect();
        let key = m["annotations"]["imgbuild.key"].as_str().unwrap_or(key).to_string();
        Ok(Some(State { layers, cfg, key }))
    }

    async fn blob_file(self: &Arc<Self>, b: &Blob) -> Result<PathBuf> {
        if let Some(p) = &b.local {
            return Ok(p.clone());
        }
        let c = cell(&self.blobs, b.digest.clone());
        c.get_or_try_init(|| async {
            let dir = node_cache("blobs").unwrap_or_else(|| self.o.work.join("blobs"));
            tokio::fs::create_dir_all(&dir).await?;
            let p = dir.join(b.digest.trim_start_matches("sha256:"));
            if p.exists() {
                touch(&p);
            } else {
                let _s = crate::prof::span("blob-download", || format!("{} {:.1} MB", &b.digest[7..19], b.size as f64 / 1e6));
                self.reg(&b.host).get_blob_file(&b.repo, &b.digest, &p).await?;
            }
            Ok::<_, anyhow::Error>(p)
        }).await.cloned()
    }

    /// Puts the blob into host/repo once per run, however many images share it.
    async fn ensure_blob(self: &Arc<Self>, host: &str, repo: &str, b: &Blob) -> Result<()> {
        let c = cell(&self.uploads, format!("{host}/{repo}@{}", b.digest));
        c.get_or_init(|| async { self.upload_blob(host, repo, b).await.map_err(|e| format!("{e:#}")) }).await.clone().map_err(anyhow::Error::msg)
    }

    async fn upload_blob(self: &Arc<Self>, host: &str, repo: &str, b: &Blob) -> Result<()> {
        let mut sp = crate::prof::span("push-blob", || format!("{repo} {} {:.1} MB", &b.digest[7..19], b.size as f64 / 1e6));
        let reg = self.reg(host);
        if reg.blob_exists(repo, &b.digest).await? {
            crate::prof::note(&mut sp, "exists");
            return Ok(());
        }
        let mut from = vec![];
        if b.local.is_none() && b.host == host {
            from.push(b.repo.as_str());
        }
        // A layer built in this run is in the cache repository only if a RUN after it got published.
        let in_cache = b.local.is_none() || self.uploads.lock().unwrap()
            .get(&format!("{}/{}@{}", self.cache.host, self.cache.repo, b.digest)).is_some_and(|c| matches!(c.get(), Some(Ok(()))));
        if self.cache.host == host && self.cache.repo != repo && in_cache {
            from.push(self.cache.repo.as_str());
        }
        for f in from {
            if reg.mount(repo, &b.digest, f).await? {
                crate::prof::note(&mut sp, "mounted");
                return Ok(());
            }
        }
        let f = self.blob_file(b).await?;
        crate::prof::note(&mut sp, "uploaded");
        let copy = self.live.clone().filter(|_| host == self.dest.host && repo == self.dest.repo).map(|live| {
            let (repo, digest, f) = (repo.to_string(), b.digest.clone(), f.clone());
            tokio::spawn(async move {
                if let Err(e) = live.blob_file(&repo, &digest, &f).await {
                    eprintln!("live: {e:#}");
                }
            })
        });
        let log = self.early_run.is_some() && host == self.dest.host && repo == self.dest.repo;
        if log {
            eprintln!("live upload: start {}", b.digest);
        }
        let pushed = reg.put_blob_file(repo, &b.digest, &f).await;
        if log && pushed.is_ok() {
            eprintln!("live upload: done {}", b.digest);
        }
        if let Some(copy) = copy {
            // Pulls that started on the copy finish on it: let it end, the registry has the layer by now.
            let _ = tokio::time::timeout(std::time::Duration::from_secs(120), copy).await;
        }
        pushed
    }

    fn upload_early(self: &Arc<Self>, platform: &str, first: usize, layers: &[Blob]) {
        let Some(slots) = &self.early_run else { return };
        for (i, blob) in layers.iter().enumerate().filter(|(_, blob)| blob.local.is_some()) {
            let Ok(slot) = slots.clone().try_acquire_owned() else { break };
            let (me, blob, platform) = (self.clone(), blob.clone(), platform.to_string());
            tokio::spawn(async move {
                let _slot = slot;
                if let Some(live) = &me.live {
                    let tag_hash = sha(me.dest.reference.as_bytes());
                    let ordinal = first + i;
                    let tag = format!("prefetch-{}-{}-{ordinal}", arch_of(&platform), &tag_hash[7..39]);
                    let announce = live.prefetch(&me.dest.repo, &tag, &platform, &blob.digest, &blob.diff_id, blob.size, &blob.media_type);
                    if !matches!(tokio::time::timeout(std::time::Duration::from_secs(2), announce).await, Ok(Ok(()))) {
                        eprintln!("live prefetch: could not announce {tag}");
                    }
                }
                let _ = me.ensure_blob(&me.dest.host, &me.dest.repo, &blob).await;
            });
        }
    }

    /// Pushes `cfg` and a manifest over `layers` into host/repo; returns the manifest bytes and digest.
    /// With `progress` (a step number), logs each phase as `docker push` does.
    #[allow(clippy::too_many_arguments)]
    async fn push_image(self: &Arc<Self>, host: &str, repo: &str, layers: &[Blob], cfg: &Value, tag: Option<&str>, key: Option<&str>, progress: Option<usize>) -> Result<(Bytes, String)> {
        // Registry round trips are slow, so layers from a registry are assumed to be in
        // `repo` already; if one is not, the manifest is refused and every layer is checked before retrying.
        let ensure = |all: bool| {
            let jobs: Vec<_> = layers.iter().filter(|b| all || b.local.is_some()).cloned().map(|b| {
                let (me, host, repo) = (self.clone(), host.to_string(), repo.to_string());
                async move { me.ensure_blob(&host, &repo, &b).await }.boxed()
            }).collect();
            stream::iter(jobs).buffer_unordered(16).try_collect::<Vec<_>>()
        };
        let reg = self.reg(host);
        let cb = Bytes::from(serde_json::to_vec(cfg)?);
        let cd = sha(&cb);
        let mut m = json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {"mediaType": "application/vnd.oci.image.config.v1+json", "digest": cd, "size": cb.len()},
            "layers": layers.iter().map(|l| {
                let mut d = json!({"mediaType": oci_layer_type(&l.media_type), "digest": l.digest, "size": l.size});
                if key.is_some() {
                    d["annotations"] = json!({"imgbuild.diff_id": l.diff_id});
                    if let Some(dirs) = &l.dirs {
                        d["annotations"]["imgbuild.dirs"] = json!(dirs_json(dirs));
                    }
                }
                d
            }).collect::<Vec<_>>(),
        });
        if let Some(k) = key {
            m["annotations"] = json!({"imgbuild.key": k});
        }
        let mb = Bytes::from(serde_json::to_vec(&m)?);
        let md = sha(&mb);
        let mt = "application/vnd.oci.image.manifest.v1+json";
        if key.is_none() && let Some(live) = &self.live {
            // The layer digests are known before any layer is uploaded: a pull that starts now needs only the layers.
            let announced = async {
                live.blob_bytes(repo, &cd, cb.clone()).await?;
                if let Some(tag) = tag {
                    live.manifest(repo, tag, mt, mb.clone()).await?;
                }
                live.manifest(repo, &md, mt, mb.clone()).await
            };
            if let Err(e) = announced.await {
                eprintln!("live: {e:#}");
            }
        }
        let t = Instant::now();
        let sp = crate::prof::span("push-layers", || format!("{repo}:{}", tag.unwrap_or("")));
        tokio::try_join!(ensure(false), reg.put_blob(repo, &cd, cb.clone()))?;
        drop(sp);
        if let Some(id) = progress {
            eprintln!("#{id} pushing layers {:.1}s done", t.elapsed().as_secs_f64());
        }
        let t = Instant::now();
        let _s = crate::prof::span("push-manifest", || format!("{repo}:{}", tag.unwrap_or(&md)));
        if reg.put_manifest(repo, tag.unwrap_or(&md), mt, mb.clone()).await.is_err() {
            // Tags are immutable: a concurrent build with the same step key, or an earlier run's part of this
            // image, got there first. What the tag holds is what an index may point to, not what was built here.
            if let Some(t) = tag
                && let Some(old) = reg.manifest_opt(repo, t).await? {
                    let d = sha(&old);
                    return Ok((old, d));
                }
            ensure(true).await?;
            reg.put_manifest(repo, tag.unwrap_or(&md), mt, mb.clone()).await?;
        }
        if let Some(id) = progress {
            eprintln!("#{id} pushing manifest for {host}/{repo}:{} {:.1}s done", tag.unwrap_or(&md), t.elapsed().as_secs_f64());
        }
        Ok((mb, md))
    }

    /// Keeps the state after a step for later builds. Each CPU's builds run on one node, so the node's cache
    /// holds them: registry writes add latency, and the image's own layers are pushed once, at the
    /// end. A RUN that starts from a state publishes it to the cache repository then, for docker to pull.
    /// `record` off the critical path: nothing later in this build reads what it saves (the node cache is for
    /// the next build), and on a busy disk its links and renames in the shared `steps/` and `blobs/` directories
    /// queue behind other jobs' journal commits for seconds. `hold` (a step lock) is released once it is saved,
    /// so another build waiting for this step still finds it; `build` waits for all of them before it removes
    /// the scratch directory they link from.
    async fn record_bg<T: Send + 'static>(self: &Arc<Self>, s: &State, lookup: &str, hold: T) -> Result<()> {
        if node_cache("steps").is_none() {
            return self.record(s, lookup).await;
        }
        let (me, s, lookup) = (self.clone(), s.clone(), lookup.to_string());
        let h = tokio::spawn(async move {
            let r = me.record(&s, &lookup).await;
            drop(hold);
            r
        });
        RECORDING.lock().unwrap().push(h);
        Ok(())
    }

    async fn record(self: &Arc<Self>, s: &State, lookup: &str) -> Result<()> {
        if node_cache("steps").is_none() {
            return self.publish(s, lookup).await.map(|_| ());
        }
        let (s, lookup) = (s.clone(), lookup.to_string());
        let _s = crate::prof::span("record", || lookup[..12].to_string());
        tokio::task::spawn_blocking(move || save_local_step(&s, &lookup)).await?
    }

    /// The state as `c-<lookup>` in the cache repository (pushed if missing).
    async fn publish(self: &Arc<Self>, s: &State, lookup: &str) -> Result<String> {
        let c = cell(&self.published, lookup.to_string());
        c.get_or_try_init(|| async {
            let tag = format!("c-{lookup}");
            let _s = crate::prof::span("publish", || tag[..14].to_string());
            if !self.reg(&self.cache.host).manifest_exists(&self.cache.repo, &tag).await? {
                // Every layer's directories go into the manifest, so a later COPY onto this state never reads layers.
                let mut layers = s.layers.clone();
                for b in layers.iter_mut().filter(|b| b.dirs.is_none()) {
                    b.dirs = Some(self.layer_dirs(b).await?);
                }
                self.push_image(&self.cache.host, &self.cache.repo, &layers, &s.cfg, Some(&tag), Some(&s.key), None).await?;
            }
            Ok::<_, anyhow::Error>(format!("{}/{}:{tag}", self.cache.host, self.cache.repo))
        }).await.cloned()
    }

    fn state<'a>(self: &'a Arc<Self>, idx: usize, tp: &'a str) -> BoxFuture<'a, Result<State>> {
        async move {
            // A failure is kept too: every stage waiting on this one fails with it instead of building it again.
            let c = cell(&self.states, format!("{idx}|{tp}"));
            c.get_or_init(|| async { self.build_stage(idx, tp).await.map_err(|e| format!("{e:#}")) }).await.clone().map_err(anyhow::Error::msg)
        }.boxed()
    }

    async fn source(self: &Arc<Self>, src: &Src, tp: &str) -> Result<State> {
        match src {
            Src::Stage(j) => self.state(*j, tp).await,
            Src::Image(r) => self.image(r, tp).await,
            Src::Context => unreachable!(),
        }
    }

    /// Walks the stage forward. Context COPYs are rebuilt (cheap); a RUN or `COPY --from` is looked up as
    /// `c-<parent key, instruction, input>` and only executed when that is new. A `COPY --from` result is keyed
    /// by its content, so the steps after it stay cached when the source stage changed but the copied files did not.
    async fn build_stage(self: &Arc<Self>, idx: usize, tp: &str) -> Result<State> {
        let pl = crate::prof::span("plan", || format!("stage {idx} {tp}"));
        let plan = self.plan(idx, tp).await?;
        drop(pl);
        let sp = plan.platform.as_str();
        let label = format!("{} {}", arch_of(tp), plan.name);
        // Resume after the last expensive step whose input key is cached; steps before it, and the stages they
        // read, are not looked at.
        let expensive: Vec<usize> = (0..plan.steps.len()).filter(|&i| !matches!(plan.steps[i].op, Op::Config | Op::Copy { src: Src::Context, .. })).collect();
        let hits = try_join_all(expensive.iter().map(|&i| self.cache_get(&plan.steps[i].ikey))).await?;
        let resume = expensive.iter().zip(hits).filter_map(|(&i, h)| h.map(|s| (i, s))).next_back();
        let start = resume.as_ref().map_or(0, |(i, _)| i + 1);
        // Start the stages this one copies from now, so independent stages build in parallel.
        for s in &plan.steps[start..] {
            if let Op::Copy { src: Src::Stage(j), .. } = &s.op {
                let (me, tp, j) = (self.clone(), tp.to_string(), *j);
                tokio::spawn(async move { me.state(j, &tp).await.ok() });
            }
        }
        let mut state = match resume {
            Some((i, mut st)) => {
                st.cfg = with_diff_ids(&plan.steps[i].cfg, &st.layers);
                st
            }
            None => {
                let mut st = match &plan.base {
                    Base::Scratch => State { layers: vec![], cfg: Value::Null, key: h(&[FORMAT, "scratch", sp]) },
                    Base::Image(s) => s.clone(),
                    Base::Stage(j) => self.state(*j, tp).await?,
                };
                st.cfg = with_diff_ids(&plan.init_cfg, &st.layers);
                st
            }
        };
        let mut wanted: HashMap<String, Vec<String>> = HashMap::new();
        for s in &plan.steps {
            if let Op::Copy { src: src @ (Src::Stage(_) | Src::Image(_)), sources, .. } = &s.op {
                wanted.entry(src_id(src)).or_default().extend(sources.iter().map(|s| prefix(s)));
            }
        }
        let n = plan.steps.len();
        for (i, s) in plan.steps.iter().enumerate().take(start) {
            if !matches!(s.op, Op::Config) {
                step_cached(&label, i, n, &s.text);
            }
        }
        // Context COPYs not built yet: a later cached step already holds their layers.
        let mut pending: Vec<(usize, &Step)> = vec![];
        let cached_all = |pending: &mut Vec<(usize, &Step)>| pending.drain(..).for_each(|(i, s)| step_cached(&label, i, n, &s.text));
        for (i, s) in plan.steps.iter().enumerate().skip(start) {
            match &s.op {
                Op::Config => state.key = h(&[&state.key, sp, &s.text]),
                Op::Copy { src: Src::Context, .. } => {
                    pending.push((i, s));
                    state.key = h(&[&state.key, sp, &s.text, &s.extra]);
                }
                Op::Copy { src, sources, dest, chown, chmod } => {
                    let from = self.source(src, tp).await?;
                    let lookup = h(&[&state.key, sp, &s.text, &from.key]);
                    if let Some(c) = self.cache_get(&lookup).await? {
                        state = c;
                        cached_all(&mut pending);
                        step_cached(&label, i, n, &s.text);
                    } else {
                        self.flush(&mut state, &mut pending, &label, n, (idx == self.target).then_some(tp)).await?;
                        step_open(&label, i, n, &s.text);
                        let seed_key = h(&["copy-seed", &state.key, sp, &s.text]);
                        let chown = self.ids(&state, chown.as_ref()).await?;
                        let dest = &self.copy_dest(&state, dest).await?;
                        let n0 = state.layers.len();
                        let inc = match src {
                            Src::Stage(_) => self.copy_incremental(&state, &from, &seed_key, sources, dest, chown, *chmod).await?,
                            _ => None,
                        };
                        if let Some((out, added)) = inc {
                            step_done(&label, &format!(" (incremental, {:.1} MB)", added as f64 / 1e6));
                            state.layers.extend(out);
                        } else {
                        let tc = Instant::now();
                        let fs = self.materialize(&from, &wanted[&src_id(src)]).await?;
                        let tm = tc.elapsed().as_secs_f64();
                        let (sources, d, chmod) = (sources.clone(), dest.clone(), *chmod);
                        let sp = crate::prof::span("copy-entries", || s.text.chars().take(60).collect());
                        let e = tokio::task::spawn_blocking(move || copy_entries(&fs.dir, Some(&fs.files), Some(&fs.meta), None, &sources, &d, chown, chmod)).await??;
                        drop(sp);
                        let te = tc.elapsed().as_secs_f64();
                        let split = std::env::var("IMGBUILD_SPLIT").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
                        let bs = self.make_layer(&state, e, dest, chown, chmod, split, false).await?;
                        if std::env::var_os("IMGBUILD_TRACE").is_some() {
                            eprintln!("  trace COPY: materialize {tm:.2}s, entries {:.2}s, layer {:.2}s", te - tm, tc.elapsed().as_secs_f64() - te);
                        }
                        step_done(&label, &format!(" ({:.1} MB)", bs.iter().map(|b| b.size).sum::<u64>() as f64 / 1e6));
                        state.layers.extend(bs);
                        }
                        if idx == self.target {
                            self.upload_early(tp, n0, &state.layers[n0..]);
                        }
                        // The layers' uncompressed digests are the content key: they cover every path, owner, mode and byte.
                        let ids: Vec<&str> = state.layers[n0..].iter().map(|b| b.diff_id.as_str()).collect();
                        let content = if ids.len() == 1 { ids[0].to_string() } else { h(&ids) };
                        if let Some(d) = node_cache("copyseeds") {
                            let v = json!({"step": lookup, "n": ids.len(), "src": from.layers.iter().map(|b| &b.digest).collect::<Vec<_>>()});
                            put_bg(d.join(&seed_key), v.to_string().into_bytes());
                        }
                        state.key = h(&[&state.key, sp, &s.text, &content]);
                        state.cfg = with_diff_ids(&s.cfg, &state.layers);
                        self.record_bg(&state, &lookup, ()).await?;
                    }
                }
                Op::Run { argv, env, mounts, secrets, network } => {
                    let lookup = h(&[&state.key, sp, &s.text]);
                    let mut hit = self.cache_get(&lookup).await?;
                    let mut lock = None;
                    if hit.is_none() {
                        if let Some(dir) = node_cache("locks") {
                            lock = Some(lock_run(&dir, &lookup, &label).await?);
                        }
                        hit = self.cache_get(&lookup).await?;
                    }
                    if let Some(c) = hit {
                        state = c;
                        cached_all(&mut pending);
                        step_cached(&label, i, n, &s.text);
                    } else {
                        self.flush(&mut state, &mut pending, &label, n, (idx == self.target).then_some(tp)).await?;
                        step_open(&label, i, n, &s.text);
                        let blobs = self.run(&state, &plan, s, &lookup, argv, env, mounts, secrets, network.as_deref(), &label, idx == self.target).await?;
                        if idx == self.target {
                            self.upload_early(tp, state.layers.len(), &blobs);
                        }
                        state.layers.extend(blobs);
                        state.key = lookup.clone();
                        state.cfg = with_diff_ids(&s.cfg, &state.layers);
                        self.record_bg(&state, &lookup, lock).await?;
                    }
                }
            }
            state.cfg = with_diff_ids(&s.cfg, &state.layers);
            if !matches!(s.op, Op::Config | Op::Copy { src: Src::Context, .. }) {
                self.record_bg(&state, &s.ikey, ()).await?;
            }
        }
        self.flush(&mut state, &mut pending, &label, n, (idx == self.target).then_some(tp)).await?;
        Ok(state)
    }

    /// Builds the pending context COPY layers onto `state`; its key already covers them.
    async fn flush(self: &Arc<Self>, state: &mut State, pending: &mut Vec<(usize, &Step)>, label: &str, n: usize, final_platform: Option<&str>) -> Result<()> {
        for (i, s) in pending.drain(..) {
            step_open(label, i, n, &s.text);
            let Op::Copy { sources, dest, chown, chmod, .. } = &s.op else { unreachable!() };
            let chown = self.ids(state, chown.as_ref()).await?;
            let dest = &self.copy_dest(state, dest).await?;
            // The same files copied to the same place make the same layer, whatever is below: kept on this
            // machine by content, so an unchanged COPY after a changed step is not read and packed again.
            let comps: Vec<&str> = dest.split('/').filter(|c| !c.is_empty()).collect();
            let anc: Vec<String> = (1..=comps.len()).map(|n| comps[..n].join("/")).collect();
            let anc_meta: BTreeMap<String, [u64; 4]> = self.dir_meta(state, &anc).await?.into_iter().map(|(k, m)| (k, [m.uid, m.gid, m.mode as u64, m.mtime])).collect();
            let memo = format!("ctx-{}", h(&[FORMAT, "source mtimes", &s.text, &s.extra, &format!("{chown:?} {chmod:?} {} {CONTEXT_MTIME}", self.o.epoch), &serde_json::to_string(&anc_meta)?]));
            let b = match local_step(&memo).and_then(|m| m.layers.into_iter().next()) {
                Some(b) => b,
                None => {
                    let (root, sources, d, chmod) = (self.o.context.clone(), sources.clone(), dest.clone(), *chmod);
                    let me = self.clone();
                    let sp = crate::prof::span("copy-entries", || s.text.chars().take(60).collect());
                    let e = tokio::task::spawn_blocking(move || copy_entries(&root, None, None, Some(&me.ignore), &sources, &d, chown, chmod)).await??;
                    drop(sp);
                    let b = self.make_layer(state, e, dest, chown, chmod, 0, true).await?.remove(0);
                    let st = State { layers: vec![b.clone()], cfg: Value::Null, key: memo.clone() };
                    self.record_bg(&st, &memo, ()).await?;
                    b
                }
            };
            step_done(label, &format!(" ({:.1} MB)", b.size as f64 / 1e6));
            if let Some(platform) = final_platform {
                if self.early_run.is_some() {
                    self.upload_early(platform, state.layers.len(), std::slice::from_ref(&b));
                } else {
                    let (me, bb) = (self.clone(), b.clone());
                    tokio::spawn(async move { me.ensure_blob(&me.dest.host, &me.dest.repo, &bb).await.ok() });
                }
            }
            state.layers.push(b);
        }
        state.cfg = with_diff_ids(&state.cfg, &state.layers);
        Ok(())
    }

    /// `dest`, ending in `/` when it is a directory already: as BuildKit, a file copied onto one goes inside it.
    async fn copy_dest(self: &Arc<Self>, state: &State, dest: &str) -> Result<String> {
        let d = dest.trim_matches('/').to_string();
        Ok(if dest.ends_with('/') || d.is_empty() || self.dir_meta(state, std::slice::from_ref(&d)).await?.is_empty() { dest.to_string() } else { format!("{dest}/") })
    }

    /// The metadata of directories `paths` in `state`, those it has.
    async fn dir_meta(self: &Arc<Self>, state: &State, paths: &[String]) -> Result<HashMap<String, Meta>> {
        let mut found: HashMap<String, Meta> = HashMap::new();
        for b in state.layers.iter().rev() {
            if found.len() == paths.len() {
                break;
            }
            let dirs = match &b.dirs {
                Some(d) => d.clone(),
                None => self.layer_dirs(b).await?,
            };
            for p in paths {
                if !found.contains_key(p)
                    && let Some(m) = dirs.get(p) {
                        found.insert(p.clone(), *m);
                    }
            }
        }
        Ok(found)
    }

    /// A `COPY --from` as the earlier copy's layers (from its last build on this machine, `copy-seed`) plus
    /// what the source stage's new layers changed under the source, when only appended layers touch it (an
    /// incremental RUN's delta). The image then has a layer more instead of the copy redone. With the layers
    /// and the size of the new ones; None when that does not apply.
    #[allow(clippy::too_many_arguments)]
    async fn copy_incremental(self: &Arc<Self>, state: &State, from: &State, seed_key: &str, sources: &[String], dest: &str,
        chown: Option<(u64, u64)>, chmod: Option<u32>) -> Result<Option<(Vec<Blob>, u64)>> {
        let Some(seed) = node_cache("copyseeds").and_then(|d| timed("copyseed-read", || fs::read(d.join(seed_key)).ok())).and_then(|b| serde_json::from_slice::<Value>(&b).ok()) else { return Ok(None) };
        let (Some(step), Some(n), Some(old)) = (seed["step"].as_str(), seed["n"].as_u64(), seed["src"].as_array()) else { return Ok(None) };
        let old: Vec<String> = old.iter().filter_map(|v| v.as_str().map(String::from)).collect();
        let n = n as usize;
        let prefixes: Vec<String> = sources.iter().map(|s| prefix(s)).collect();
        // ponytail: at most 24 layers per copy, then it is redone (overlayfs stacks ~120 layers).
        if prefixes.iter().any(String::is_empty) || n == 0 || n >= 24 {
            return Ok(None);
        }
        let Some(prev) = local_step(step).filter(|p| p.layers.len() >= n) else { return Ok(None) };
        let old_out = prev.layers[prev.layers.len() - n..].to_vec();
        let new_set: std::collections::HashSet<&str> = from.layers.iter().map(|b| b.digest.as_str()).collect();
        let old_set: std::collections::HashSet<&str> = old.iter().map(String::as_str).collect();
        let blobs = node_cache("blobs").context("no node cache")?;
        let gone: Vec<PathBuf> = old.iter().filter(|d| !new_set.contains(d.as_str())).map(|d| blobs.join(d.trim_start_matches("sha256:"))).collect();
        let added: Vec<&Blob> = from.layers.iter().filter(|b| !old_set.contains(b.digest.as_str())).collect();
        let size: u64 = added.iter().map(|b| b.size).sum::<u64>() + gone.iter().map(|p| fs::metadata(p).map_or(u64::MAX / 4, |m| m.len())).sum::<u64>();
        if size > 256 << 20 {
            return Ok(None);
        }
        // Into a directory, a file source lands under its own name, which the mapping below does not do.
        if dest.ends_with('/') && self.dir_meta(from, std::slice::from_ref(&prefixes[0])).await?.is_empty() {
            return Ok(None);
        }
        let map = match (sources, is_glob(&sources[0])) {
            ([_], false) => {
                let dest = dest.trim_matches('/').to_string();
                let fresh = !dest.is_empty() && self.dir_meta(state, std::slice::from_ref(&dest)).await?.is_empty();
                Some(CopyMap { src: prefixes[0].clone(), dest, chown, chmod, fresh })
            }
            _ => None,
        };
        let mut added_files = vec![];
        for b in &added {
            added_files.push(self.blob_file(b).await?);
        }
        let work = scratch(&self.o.work).join("layers");
        let _s = crate::prof::span("copy-delta", || format!("{} new layers", added_files.len()));
        let r = tokio::task::spawn_blocking(move || -> Result<Option<Deltas>> {
            let mut gone_touched = vec![];
            for p in &gone {
                gone_touched.push(!matches!(copy_delta(p, &prefixes, None, &work)?, Delta::Untouched));
            }
            let mut out = vec![];
            for f in &added_files {
                out.push((f.to_string_lossy().into_owned(), copy_delta(f, &prefixes, map.as_ref(), &work)?));
            }
            Ok(Some((out, gone_touched)))
        }).await??;
        let Some((deltas, gone_touched)) = r else { return Ok(None) };
        if gone_touched.iter().any(|t| *t) || deltas.iter().any(|(_, d)| matches!(d, Delta::Unsafe)) {
            return Ok(None);
        }
        // The layers that matter, in order, must be the earlier ones followed by the new touching ones.
        let touched: std::collections::HashMap<&str, &Delta> = added.iter().zip(&deltas).map(|(b, (_, d))| (b.digest.as_str(), d)).collect();
        let kept: Vec<&str> = from.layers.iter().map(|b| b.digest.as_str()).filter(|d| !matches!(touched.get(d), Some(Delta::Untouched))).collect();
        let old_kept: Vec<&str> = old.iter().map(String::as_str).filter(|d| new_set.contains(d)).collect();
        if !kept.starts_with(&old_kept) || kept[old_kept.len()..].iter().any(|d| !touched.contains_key(d)) {
            return Ok(None);
        }
        let new: Vec<PathBuf> = kept[old_kept.len()..].iter().filter_map(|d| match touched.get(d) { Some(Delta::Layer(l, _)) => Some(l.path.clone()), _ => None }).collect();
        let mut old_files = vec![];
        for b in &old_out {
            old_files.push(self.blob_file(b).await?);
        }
        let same = {
            let _s = crate::prof::span("copy-same", || format!("{} deltas against {} layers", new.len(), old_files.len()));
            tokio::task::spawn_blocking(move || new.iter().try_fold(true, |all, l| anyhow::Ok(all && redundant(l, &old_files)?))).await??
        };
        if same {
            return Ok(Some((old_out, 0)));
        }
        let mut out = old_out;
        let mut size = 0;
        for d in &kept[old_kept.len()..] {
            if let Some(Delta::Layer(l, dirs)) = touched.get(d) {
                size += l.size;
                out.push(local_blob(crate::layer::Layer { path: l.path.clone(), digest: l.digest.clone(), diff_id: l.diff_id.clone(), size: l.size }, dirs.clone()));
            }
        }
        Ok(Some((out, size)))
    }

    /// A layer of `entries`, preceded by their parent directories as they are in `state`: a layer without
    /// them gets them created as 0755 root by Docker, which in overlayfs hides e.g. /tmp's 1777.
    /// Missing directories (the destination and its parents) are created as BuildKit does: `--chown` owner
    /// (else root), `--chmod` mode (else 0755); existing ones are kept as they are.
    #[allow(clippy::too_many_arguments)]
    async fn make_layer(self: &Arc<Self>, state: &State, entries: Vec<Entry>, dest: &str, chown: Option<(u64, u64)>, chmod: Option<u32>, split: usize, ctx: bool) -> Result<Vec<Blob>> {
        let have: std::collections::HashSet<&str> = entries.iter().map(|e| e.path.as_str()).collect();
        // The destination counts too (an empty source directory still creates it), unless it is the copied file.
        let dest = norm_path(Path::new(dest));
        let mut parents: Vec<String> = entries.iter().map(|e| e.path.as_str()).chain((!dest.is_empty()).then_some(format!("{dest}/x")).as_deref()).flat_map(|p| {
            let c: Vec<&str> = p.split('/').collect();
            (1..c.len()).map(move |n| c[..n].join("/")).collect::<Vec<_>>()
        }).filter(|p| !have.contains(p.as_str())).collect();
        parents.sort();
        parents.dedup();
        let found = self.dir_meta(state, &parents).await?;
        let epoch = self.o.epoch;
        // A missing working directory (or a parent of it) is what WORKDIR creates, owned by the user.
        // ponytail: BuildKit uses the USER as of the WORKDIR line; this takes the USER of the step.
        let o = &state.cfg["config"];
        let wd = o["WorkingDir"].as_str().unwrap_or("").trim_matches('/').to_string();
        let user = o["User"].as_str().filter(|u| !u.is_empty()).map(|u| u.split_once(':').map_or((u.to_string(), String::new()), |(a, b)| (a.to_string(), b.to_string())));
        let wd_owner = match user {
            Some(u) if parents.iter().any(|p| !found.contains_key(p) && is_under(&wd, p)) => self.ids(state, Some(&u)).await?,
            _ => None,
        };
        let mut all: Vec<Entry> = parents.iter().map(|p| Entry {
            path: p.clone(),
            kind: Kind::Dir,
            meta: found.get(p).copied().unwrap_or_else(|| {
                match wd_owner.filter(|_| is_under(&wd, p)) {
                    Some((uid, gid)) => Meta { uid, gid, mode: 0o755, mtime: epoch },
                    None => {
                        let (uid, gid) = chown.unwrap_or((0, 0));
                        Meta { uid, gid, mode: chmod.unwrap_or(0o755), mtime: epoch }
                    }
                }
            }),
        }).collect();
        all.extend(entries);
        let epoch = if ctx { CONTEXT_MTIME } else { epoch };
        if ctx {
            for_context(&mut all);
        }
        canonical(&mut all, epoch);
        let work = scratch(&self.o.work).join("layers");
        let mut sp = crate::prof::span("layer-write", || format!("{} entries", all.len()));
        let groups = if all.len() >= 1500 { split_by_package(all, split) } else { vec![all] };
        let mut out = vec![];
        let mut size = 0;
        for mut g in groups {
            canonical(&mut g, epoch);
            let work = work.clone();
            let (l, dirs) = tokio::task::spawn_blocking(move || anyhow::Ok((write_layer(&g, &work)?, dirs_of(&g)))).await??;
            size += l.size;
            out.push(local_blob(l, dirs));
        }
        crate::prof::note(&mut sp, &format!("{:.1} MB in {} layers", size as f64 / 1e6, out.len()));
        Ok(out)
    }

    /// Numeric ids for a `--chown`, names looked up in the image's /etc/passwd and /etc/group. The answer
    /// is kept on disk under the state key, so a context COPY rebuilt every run reads the layers only once.
    async fn ids(self: &Arc<Self>, state: &State, chown: Option<&(String, String)>) -> Result<Option<(u64, u64)>> {
        let Some((u, g)) = chown else { return Ok(None) };
        let num = |s: &str| if s == "root" { Some(0) } else { s.parse::<u64>().ok() };
        if let (Some(uid), Some(gid)) = (num(u), if g.is_empty() { num(u) } else { num(g) }) {
            return Ok(Some((uid, gid)));
        }
        let memo = std::env::var_os("IMGBUILD_CACHE_DIR").map(|d| PathBuf::from(d).join(format!("ids-{}", h(&[&state.key, u, g]))));
        if let Some(t) = memo.as_ref().and_then(|p| timed("ids-read", || fs::read_to_string(p).ok()))
            && let Some((a, b)) = t.split_once(' ').and_then(|(a, b)| Some((a.parse().ok()?, b.trim().parse().ok()?))) {
                return Ok(Some((a, b)));
            }
        let fs = self.materialize(state, &["etc/passwd".into(), "etc/group".into()]).await?;
        let find = |file: &str, name: &str, col: usize| -> Result<u64> {
            let t = fs.files.read(file).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
            t.lines().map(|l| l.split(':').collect::<Vec<_>>()).find(|f| f[0] == name && f.len() > col)
                .and_then(|f| f[col].parse().ok()).with_context(|| format!("--chown: {name} is not in /{file}"))
        };
        let uid = match num(u) { Some(n) => n, None => find("etc/passwd", u, 2)? };
        let gid = if g.is_empty() { uid } else { match num(g) { Some(n) => n, None => find("etc/group", g, 2)? } };
        if let Some(p) = memo {
            put_bg(p, format!("{uid} {gid}").into_bytes());
        }
        Ok(Some((uid, gid)))
    }

    async fn layer_dirs(self: &Arc<Self>, b: &Blob) -> Result<Dirs> {
        let c = cell(&self.dirs, b.digest.clone());
        c.get_or_try_init(|| async {
            let memo = node_cache("dirs").map(|d| d.join(b.digest.trim_start_matches("sha256:")));
            if let Some(d) = memo.as_ref().and_then(|p| timed("dirs-read", || fs::read_to_string(p).ok())).and_then(|t| dirs_from(&json!({"annotations": {"imgbuild.dirs": t}}))) {
                return Ok(d);
            }
            let f = self.blob_file(b).await?;
            let _s = crate::prof::span("scan-dirs", || format!("{} {:.1} MB", &b.digest[7..19], b.size as f64 / 1e6));
            let d: Dirs = Arc::new(tokio::task::spawn_blocking(move || scan_dirs(&f)).await??);
            if let Some(p) = memo {
                put_bg(p, dirs_json(&d).into_bytes());
            }
            Ok::<_, anyhow::Error>(d)
        }).await.cloned()
    }

    async fn materialize(self: &Arc<Self>, from: &State, prefixes: &[String]) -> Result<Arc<Rootfs>> {
        let mut prefixes = prefixes.to_vec();
        prefixes.sort();
        prefixes.dedup();
        if prefixes.iter().any(String::is_empty) {
            prefixes = vec![String::new()];
        }
        let id = h(&[&from.key, &prefixes.join("\0")]);
        let c = cell(&self.rootfs, id.clone());
        c.get_or_try_init(|| async {
            let files = try_join_all(from.layers.iter().cloned().map(|b| {
                let me = self.clone();
                async move { me.blob_file(&b).await }.boxed()
            })).await?;
            let dir = scratch(&self.o.work).join("fs").join(&id[..16]);
            let d = dir.clone();
            let t = Instant::now();
            let n = files.len();
            let _s = crate::prof::span("extract", || format!("{n} layers under {:?}", prefixes.iter().take(3).collect::<Vec<_>>()));
            let (meta, files) = tokio::task::spawn_blocking(move || extract(&d, &files, &prefixes)).await??;
            if std::env::var_os("IMGBUILD_TRACE").is_some() {
                eprintln!("  trace {:>6}ms extract {n} layers for {}", t.elapsed().as_millis(), &id[..12]);
            }
            Ok::<_, anyhow::Error>(Arc::new(Rootfs { dir, meta, files }))
        }).await.cloned()
    }

    async fn docker_ready(&self) -> Result<()> {
        self.docker.get_or_try_init(|| async {
            if let (Ok(u), Ok(p)) = (std::env::var("IMGBUILD_USER"), std::env::var("IMGBUILD_PASS")) {
                let host = self.cache.host.clone();
                tokio::task::spawn_blocking(move || -> Result<()> {
                    let mut c = Command::new("docker").args(["login", &host, "-u", &u, "--password-stdin"]).stdin(Stdio::piped()).stdout(Stdio::null()).spawn()?;
                    c.stdin.take().unwrap().write_all(p.as_bytes())?;
                    ensure!(c.wait()?.success(), "docker login {host} failed");
                    Ok(())
                }).await??;
            }
            Ok::<_, anyhow::Error>(())
        }).await.map(|_| ())
    }

    async fn binfmt(&self, arch: &str) -> Result<()> {
        let c = cell(&self.binfmt, arch.to_string());
        let arch = arch.to_string();
        c.get_or_try_init(|| async move {
            tokio::task::spawn_blocking(move || {
                // ponytail: installed at most hourly per machine (registrations last until the kernel reboots);
                // after a reboot within the hour, RUNs fail with "exec format error" until the marker expires.
                let marker = node_cache(&format!("binfmt-{arch}"));
                let fresh = marker.as_ref().and_then(|m| fs::metadata(m).ok()?.modified().ok()?.elapsed().ok()).is_some_and(|a| a.as_secs() < 3600);
                if !fresh {
                    let ok = Command::new("docker").args(["run", "--privileged", "--rm", "tonistiigi/binfmt", "--install", &arch]).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success());
                    if let (true, Some(m)) = (ok, marker) {
                        let _ = fs::create_dir_all(m.parent().unwrap()).and_then(|_| fs::write(m, b""));
                    }
                }
            }).await?;
            Ok::<_, anyhow::Error>(())
        }).await.map(|_| ())
    }

    #[allow(clippy::too_many_arguments)]
    /// The layer is read straight from the container: a later RUN pulls the state from the cache repository,
    /// which for a 1.3 GB layer costs ~13 s against ~80 s for `docker commit` (it gzips on one core).
    async fn run(self: &Arc<Self>, state: &State, plan: &Plan, step: &Step, lookup: &str, argv: &[String], env: &[String], mounts: &[(String, String)], secrets: &[(String, String)],
        network: Option<&str>, label: &str, final_stage: bool) -> Result<Vec<Blob>> {
        let tl = Instant::now();
        let sp = crate::prof::span("run-login", String::new);
        self.docker_ready().await?;
        drop(sp);
        if std::env::var_os("IMGBUILD_TRACE").is_some() {
            eprintln!("  trace RUN: docker login {:.2}s", tl.elapsed().as_secs_f64());
        }
        let daemon = docker_for(arch_of(&plan.platform));
        if daemon.is_none() && arch_of(&plan.platform) != arch_of(&self.host) {
            self.binfmt(arch_of(&plan.platform)).await?;
        }
        // The runners share their node's docker daemon, and Harbor tags are immutable: both names are per run.
        let run_id = format!("{}-{}", uniq(), self.t0.elapsed().as_nanos() % 1_000_000);
        let tmp = format!("{}/{}:t-{}-{run_id}", self.cache.host, self.cache.repo, &lookup[..32]);
        let name = format!("imgbuild-{}-{run_id}", &lookup[..12]);
        let o = &state.cfg["config"];
        let mut opts: Vec<String> = vec!["--name".into(), name.clone(), "--platform".into(), plan.platform.clone(), "--pull".into(), "missing".into()];
        if let Some(u) = o["User"].as_str().filter(|u| !u.is_empty()) {
            opts.extend(["--user".into(), u.into()]);
        }
        if let Some(w) = o["WorkingDir"].as_str().filter(|w| !w.is_empty()) {
            opts.extend(["--workdir".into(), w.into()]);
        }
        if let Some(n) = network {
            opts.extend(["--network".into(), n.into()]);
        }
        for e in env {
            opts.extend(["--env".into(), e.clone()]);
        }
        if daemon.is_none() && arch_of(&plan.platform) == "arm64" && arch_of(&self.host) != "arm64" {
            // QEMU's default CPU (`max`) emulates pointer authentication and more, which CPU-bound code pays
            // 1.7-1.9x for (measured, QEMU 10.2). Neoverse N1 is what the arm64 nodes are.
            opts.extend(["--env".into(), "QEMU_CPU=neoverse-n1".into()]);
        }
        for (v, t) in mounts {
            opts.extend(["--volume".into(), format!("{v}:{t}")]);
        }
        // `--env NAME` takes the value from docker's own environment, so it is in no argument list or log.
        let secret_env: Vec<(String, String)> = secrets.iter().filter_map(|(src, var)| Some((var.clone(), std::env::var(src).ok().filter(|v| !v.is_empty())?))).collect();
        for (var, _) in &secret_env {
            opts.extend(["--env".into(), var.clone()]);
        }
        opts.extend(["--label".into(), docker_cache::RUN_LABEL.into()]);
        // A cgroup that outlives the RUN containers, so the caller can read the CPU they used (`<name>.slice` suits
        // both cgroup drivers).
        if let Some(parent) = std::env::var("IMGBUILD_CGROUP_PARENT").ok().filter(|p| !p.is_empty()) {
            opts.extend(["--cgroup-parent".into(), parent]);
        }
        opts.extend(["--entrypoint".into(), argv[0].clone()]);
        let targets: Vec<String> = mounts.iter().map(|(_, t)| t.trim_matches('/').to_string()).collect();
        let lineage = if secrets.is_empty() { self.lineage(state, plan, step, argv) } else { None };
        let seed_img = |l: &str| format!("imgbuild-seed:{}-{run_id}", &l[..16]);
        if let Some(l) = &lineage {
            let record = || node_cache("seeds").and_then(|d| fs::read(d.join(l)).ok());
            // A seed replaced meanwhile (its image then removed) is tried again, as the new one.
            for _ in 0..2 {
                let before = record();
                match self.run_incremental(state, lookup, l, &seed_img(l), &name, &opts, argv, &targets, daemon.clone(), label, final_stage).await {
                    Ok(Some(b)) => {
                        step_done(label, &format!(" (incremental, {:.1} MB)", b.last().map_or(0, |b| b.size) as f64 / 1e6));
                        return Ok(b);
                    }
                    Ok(None) if record() != before => continue,
                    Ok(None) => {}
                    Err(e) => self.log(format!("{label}: running the whole RUN instead of on the last result: {e:#}")),
                }
                break;
            }
        }
        let tl = Instant::now();
        let sp = crate::prof::span("run-load", || format!("{} layers", state.layers.len()));
        let loaded = {
            let (st, d) = (state.clone(), daemon.clone());
            tokio::task::spawn_blocking(move || load_state(&st, d.as_deref())).await?
        };
        drop(sp);
        if std::env::var_os("IMGBUILD_TRACE").is_some() {
            eprintln!("  trace RUN: load state {:.2}s", tl.elapsed().as_secs_f64());
        }
        let img = match loaded {
            Ok(Some(t)) => t,
            Ok(None) => self.publish(state, &state.key).await?,
            Err(e) => {
                self.log(format!("{label}: loading the state into docker failed, going through the registry: {e:#}"));
                self.publish(state, &state.key).await?
            }
        };
        let mut run = vec!["run".to_string()];
        run.extend(opts);
        run.push(img.clone());
        run.extend(argv[1..].iter().cloned());
        let (label_c, t0, tmp_ref) = (label.to_string(), Instant::now(), tmp.clone());
        let reload = (state.clone(), img.clone());
        let work = scratch(&self.o.work).join("layers");
        let seed = lineage.as_ref().map(|l| (l.clone(), seed_img(l), lookup.to_string()));
        let pre = state.layers.len();
        let epoch = self.o.epoch;
        let out = tokio::task::spawn_blocking(move || -> Result<RunLayer> {
            let docker = || {
                let mut c = Command::new("docker");
                if let Some(h) = &daemon {
                    c.env("DOCKER_HOST", h);
                }
                c.envs(secret_env.iter().map(|(k, v)| (k, v)));
                c
            };
            let marker = format!(".{name}");
            let upper = upper_helper(&docker, daemon.as_deref());
            let status = if upper.is_some() {
                let mut create = run.clone();
                create[0] = "create".into();
                let sp = crate::prof::span("run-create", String::new);
                // A node-wide `docker image prune -a` (ram-docker) can delete the state between load and create.
                if let Err(e) = sh(docker().args(&create)) {
                    let _ = docker().args(["rm", "-f", &name]).stdout(Stdio::null()).stderr(Stdio::null()).status();
                    let _ = docker().args(["rmi", &reload.1]).stdout(Stdio::null()).stderr(Stdio::null()).status();
                    if !matches!(load_state(&reload.0, daemon.as_deref()), Ok(Some(t)) if t == reload.1) {
                        return Err(e);
                    }
                    sh(docker().args(&create))?;
                }
                drop(sp);
                let sp = crate::prof::span("run-marker", String::new);
                if std::env::var_os("IMGBUILD_TRACE").is_some() {
                    eprintln!("  trace RUN: create (and pull) {:.2}s", t0.elapsed().as_secs_f64());
                }
                let mut cp = docker().args(["cp", "-", &format!("{name}:/")]).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()?;
                cp.stdin.take().unwrap().write_all(&marker_tar(&marker)?)?;
                ensure!(cp.wait()?.success(), "docker cp into {name}");
                drop(sp);
                let mut c = docker();
                c.args(["start", "--attach", &name]);
                let _s = crate::prof::span("run-exec", || label_c.clone());
                stream(c, &label_c)?
            } else {
                let mut c = docker();
                c.args(&run);
                stream(c, &label_c)?
            };
            if !status.success() {
                let _ = docker().args(["rm", "-f", &name]).stdout(Stdio::null()).stderr(Stdio::null()).status();
            }
            ensure!(status.success(), "RUN failed ({status})");
            let ran = t0.elapsed().as_secs_f64();
            {
                let _s = crate::prof::span("run-layer", || if upper.is_some() { "upper dir".into() } else { "container".into() });
                let l = match &upper {
                    Some(x) => layer_from_upper(&docker, x, &marker, &work, &targets, !final_stage, epoch)?,
                    None => None,
                };
                let l = match l {
                    Some(l) => Some(l),
                    None => layer_from_container(&docker, &name, &work, &targets, true, !final_stage)?,
                };
                if let Some(l) = l {
                    match &seed {
                        Some((lin, simg, step)) => {
                            let adopt = save_seed(lin, simg, step, pre, 0, daemon.as_deref());
                            keep_seed(daemon.as_deref(), &format!("docker commit --change 'LABEL {}' {name} {simg} >/dev/null && ok=1; docker rm -f {name}; [ -n \"$ok\" ] && {adopt}", docker_cache::IMAGE_LABEL));
                        }
                        None => detach(daemon.as_deref(), &format!("docker rm -f {name}")),
                    }
                    used_image(daemon.as_deref(), &img);
                    return Ok((Some(l), [ran, t0.elapsed().as_secs_f64() - ran, 0.0]));
                }
            }
            sh(docker().args(["commit", &name, &tmp]))?;
            let committed = t0.elapsed().as_secs_f64();
            let _ = docker().args(["rm", "-f", &name]).stdout(Stdio::null()).status();
            // Both images stay in the daemon: the next RUN starts from their layers instead of pulling GBs again.
            used_image(daemon.as_deref(), &img);
            used_image(daemon.as_deref(), &tmp);
            if let Some((lin, _, step)) = &seed {
                keep_seed(daemon.as_deref(), &save_seed(lin, &tmp, step, pre, 0, daemon.as_deref()));
            }
            sh(docker().args(["push", "--quiet", &tmp]))?;
            Ok((None, [ran, committed - ran, t0.elapsed().as_secs_f64() - committed]))
        }).await?;
        let (exported, [ran, commit, push]) = out?;
        if let Some((l, dirs)) = exported {
            step_done(label, &format!(" (run {ran:.1}s, layer {commit:.1}s, {:.1} MB)", l.size as f64 / 1e6));
            return Ok(vec![local_blob(l, dirs)]);
        }
        step_done(label, &format!(" (run {ran:.1}s, commit {commit:.1}s, push {push:.1}s)"));
        let pushed = self.image(&tmp_ref, &plan.platform).await?;
        if pushed.layers.len() == state.layers.len() {
            // Nothing changed, so docker committed no layer; keep one per step, as `docker build` does.
            let work = scratch(&self.o.work).join("layers");
            let l = tokio::task::spawn_blocking(move || write_layer(&[], &work)).await??;
            return Ok(vec![local_blob(l, Default::default())]);
        }
        ensure!(pushed.layers.len() == state.layers.len() + 1, "committed image has {} layers, expected {}", pushed.layers.len(), state.layers.len() + 1);
        Ok(vec![pushed.layers.last().unwrap().clone()])
    }

    /// Which RUN this is across builds on this machine, for its seed (see `run_incremental`); None when the
    /// command is not one that may run on its own earlier result.
    fn lineage(&self, state: &State, plan: &Plan, step: &Step, argv: &[String]) -> Option<String> {
        incremental(argv).then(|| h(&["seed", &plan.platform, &docker_for(arch_of(&plan.platform)).unwrap_or_default(),
            &self.o.dockerfile.to_string_lossy(), &plan.name, &step.text, &state.cfg["config"].to_string()]))
    }

    /// Runs the step in a container of its last result on this machine (the seed) with this build's changed
    /// layers copied in, so the command finds its earlier work in place (`uv sync` then installs only what
    /// changed), and reads back only what it changed. The step's layers are then the seed's RUN layers plus
    /// that change; in the final stage, which gets pushed, they are merged into one. None when there is no
    /// usable seed.
    #[allow(clippy::too_many_arguments)]
    async fn run_incremental(self: &Arc<Self>, state: &State, lookup: &str, lineage: &str, new_seed: &str, name: &str, opts: &[String],
        argv: &[String], targets: &[String], daemon: Option<String>, label: &str, final_stage: bool) -> Result<Option<Vec<Blob>>> {
        let skip = |why: String| {
            crate::prof::record("incr-skip", || format!("{label}: {why}"), Instant::now());
            Ok(None)
        };
        let Some(seed) = node_cache("seeds").and_then(|d| fs::read(d.join(lineage)).ok()).and_then(|b| serde_json::from_slice::<Value>(&b).ok()) else { return skip(format!("no seed {}", &lineage[..12])) };
        let (Some(image), Some(depth)) = (seed["image"].as_str().map(String::from), seed["depth"].as_u64()) else { return skip("seed record without image".into()) };
        let Some(old) = seed["step"].as_str().and_then(local_step) else { return skip(format!("the seed's step {} is gone from the node cache", seed["step"].as_str().unwrap_or("?").chars().take(12).collect::<String>())) };
        let pre = seed["pre"].as_u64().map_or(old.layers.len().saturating_sub(1), |p| p as usize);
        if pre == 0 || pre >= old.layers.len() || (final_stage && old.layers.len() - pre != 1) {
            return skip(format!("seed layers {} pre {pre} final {final_stage}", old.layers.len()));
        }
        let (old_pre, old_run) = old.layers.split_at(pre);
        let k = old_pre.iter().zip(&state.layers).take_while(|(a, b)| a.digest == b.digest).count();
        let tail_bytes: u64 = state.layers[k..].iter().chain(&old_pre[k..]).map(|b| b.size).sum();
        // ponytail: a seed is used only when what changed under the RUN is small (the lock files, not a new
        // base image) and its layer chain is short; overlayfs stacks at most ~120 layers.
        if k == 0 || tail_bytes > 64 << 20 || depth >= 24 {
            if std::env::var_os("IMGBUILD_TRACE").is_some() {
                eprintln!("  trace incremental: seed not used (common layers {k}, changed {tail_bytes} bytes, depth {depth})");
            }
            return skip(format!("common layers {k}, changed {tail_bytes} bytes, depth {depth}"));
        }
        let mut new_tail = vec![];
        for b in &state.layers[k..] {
            new_tail.push(self.blob_file(b).await?);
        }
        let mut old_tail = vec![];
        for b in &old_pre[k..] {
            old_tail.push(self.blob_file(b).await?);
        }
        let old_file = self.blob_file(&old_run[0]).await?;
        let (label, name, work, targets, new_seed) = (label.to_string(), name.to_string(), scratch(&self.o.work).join("layers"), targets.to_vec(), new_seed.to_string());
        // pnpm leaves packages no longer in the lock in node_modules/.pnpm for a week; a fresh install has none.
        let extra_env: Vec<String> = match argv.last().is_some_and(|c| c.contains("pnpm")) {
            true => ["npm_config_modules_cache_max_age=0", "pnpm_config_modules_cache_max_age=0"].iter().flat_map(|v| ["-e".to_string(), v.to_string()]).collect(),
            false => vec![],
        };
        let mut create = vec!["create".to_string()];
        create.extend(opts.iter().cloned());
        create.extend(extra_env.iter().cloned());
        create.extend(["--pull".into(), "never".into(), image.clone()]);
        create.extend(argv[1..].iter().cloned());
        let (lookup, lineage, n_pre) = (lookup.to_string(), lineage.to_string(), state.layers.len());
        let epoch = self.o.epoch;
        let out = tokio::task::spawn_blocking(move || -> Result<Option<(crate::layer::Layer, Dirs)>> {
            let docker = || {
                let mut c = Command::new("docker");
                if let Some(h) = &daemon {
                    c.env("DOCKER_HOST", h);
                }
                c
            };
            // Every file the seed's changed layers had is in this build's, so the copy leaves nothing stale.
            let names = |files: &[PathBuf]| -> Result<std::collections::BTreeSet<String>> {
                let mut out = std::collections::BTreeSet::new();
                for f in files {
                    for e in tar::Archive::new(open_layer(f)?).entries()? {
                        out.insert(norm_path(&e?.path()?));
                    }
                }
                Ok(out)
            };
            let sp = crate::prof::span("incr-check", || format!("{} + {} layers", old_tail.len(), new_tail.len()));
            let (old_names, new_names) = (names(&old_tail)?, names(&new_tail)?);
            drop(sp);
            let trace = std::env::var_os("IMGBUILD_TRACE").is_some();
            if !old_names.is_subset(&new_names) || new_names.iter().any(|n| n.rsplit('/').next().is_some_and(|b| b.starts_with(".wh."))) {
                let why = format!("the seed had files this build's layers do not: {:?}", old_names.difference(&new_names).take(3).collect::<Vec<_>>());
                if trace {
                    eprintln!("  trace incremental: seed not used: {why}");
                }
                crate::prof::record("incr-skip", || format!("{label}: {why}"), Instant::now());
                return Ok(None);
            }
            let t0 = Instant::now();
            let marker = format!(".{name}");
            let upper = upper_helper(&docker, daemon.as_deref());
            let copy_in = || -> Result<()> {
                let mut cp = docker().args(["cp", "--archive", "-", &format!("{name}:/")]).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped()).spawn()?;
                let mut b = tar::Builder::new(BufWriter::with_capacity(1 << 20, cp.stdin.take().unwrap()));
                for f in &new_tail {
                    for e in tar::Archive::new(open_layer(f)?).entries()? {
                        let mut e = e?;
                        let rel = e.path()?.to_string_lossy().into_owned();
                        append_entry(&mut b, &mut e, &rel)?;
                    }
                }
                if upper.is_some() {
                    let mut hd = tar::Header::new_gnu();
                    hd.set_mode(0o600);
                    hd.set_size(0);
                    b.append_data(&mut hd, &marker, io::empty())?;
                }
                b.into_inner()?.flush()?;
                let o = cp.wait_with_output()?;
                ensure!(o.status.success(), "docker cp into {name}: {}", String::from_utf8_lossy(&o.stderr).trim());
                Ok(())
            };
            // The seed image exists only once its background commit is done (`save_seed`).
            let sp = crate::prof::span("incr-create", String::new);
            let created = docker().args(&create).stdout(Stdio::null()).stderr(Stdio::null()).status()?.success();
            drop(sp);
            if !created {
                if trace {
                    eprintln!("  trace incremental: seed not used: no image {image} yet");
                }
                crate::prof::record("incr-skip", || format!("{label}: no seed image {image} in docker"), Instant::now());
                return Ok(None);
            }
            let result = (|| -> Result<Option<(crate::layer::Layer, Dirs)>> {
                {
                    let _s = crate::prof::span("incr-copy-in", String::new);
                    copy_in()?;
                }
                let copied = t0.elapsed().as_secs_f64();
                let mut c = docker();
                c.args(["start", "--attach", &name]);
                let sp = crate::prof::span("incr-exec", || label.clone());
                let status = stream(c, &label)?;
                drop(sp);
                ensure!(status.success(), "RUN failed ({status})");
                let ran = t0.elapsed().as_secs_f64();
                let sp = crate::prof::span("incr-layer", String::new);
                let d = match &upper {
                    Some(x) => layer_from_upper(&docker, x, &marker, &work, &targets, !final_stage, epoch)?,
                    None => None,
                };
                let d = match d {
                    Some(d) => Some(d),
                    None => layer_from_container(&docker, &name, &work, &targets, false, !final_stage)?,
                };
                drop(sp);
                let Some((d, dirs)) = d else {
                    crate::prof::record("incr-skip", || format!("{label}: no layer read back"), Instant::now());
                    return Ok(None);
                };
                let read = t0.elapsed().as_secs_f64();
                if !final_stage {
                    if trace {
                        eprintln!("  trace incremental: create+cp {copied:.2}s, run {:.2}s, read {:.2}s", ran - copied, read - ran);
                    }
                    return Ok(Some((d, dirs)));
                }
                let _s = crate::prof::span("incr-merge", String::new);
                let merged = merge_layer(&old_file, &d.path, &work)?;
                if trace {
                    eprintln!("  trace incremental: create+cp {copied:.2}s, run {:.2}s, read {:.2}s, merge {:.2}s", ran - copied, read - ran, t0.elapsed().as_secs_f64() - read);
                }
                let _ = fs::remove_file(&d.path);
                Ok(merged)
            })();
            match &result {
                Ok(Some(_)) => {
                    let adopt = save_seed(&lineage, &new_seed, &lookup, n_pre, depth + 1, daemon.as_deref());
                    keep_seed(daemon.as_deref(), &format!("docker commit --change 'LABEL {}' {name} {new_seed} >/dev/null && ok=1; docker rm -f {name}; [ -n \"$ok\" ] && {adopt}", docker_cache::IMAGE_LABEL));
                }
                _ => detach(daemon.as_deref(), &format!("docker rm -f {name}")),
            }
            result
        }).await??;
        Ok(out.map(|(l, dirs)| {
            let new = local_blob(l, dirs);
            if final_stage { vec![new] } else { old_run.iter().cloned().chain([new]).collect() }
        }))
    }
}

pub async fn build(mut o: Opts) -> Result<String> {
    // Absolute once: resolving a relative path calls getcwd, which costs ~100 µs on macOS, per context file.
    o.context = std::path::absolute(&o.context)?;
    let work = o.work.clone();
    let snap = crate::prof::snap(&node_cache("").unwrap_or_else(|| work.clone()));
    // Off the critical path: pruning removes GBs (files, docker images), which takes minutes on a busy disk.
    let prune = tokio::task::spawn_blocking(prune_node_cache);
    if let Some(d) = node_cache("tmp") {
        let fsname = Command::new("stat").args(["-f", "-c", "%T"]).arg(&d).output().ok().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default();
        eprintln!("profile: scratch filesystem {fsname}");
    }
    let r = build_in(o).await;
    drain_memos().await;
    let r = match (r, drain_records().await) {
        (Ok(v), Ok(())) => Ok(v),
        (Err(e), _) | (_, Err(e)) => Err(e),
    };
    {
        let _s = crate::prof::span("finish", || "save context stat cache, remove scratch".into());
        tokio::task::spawn_blocking(save_ctx).await?;
        // Extracted filesystems and built layers are only for this run (`record` keeps layers in the node cache).
        remove_scratch(&scratch(&work));
    }
    {
        let _s = crate::prof::span("prune-wait", String::new);
        prune.await?;
    }
    {
        let t = Instant::now();
        let mut sp = crate::prof::span("seed-wait", String::new);
        let done = tokio::task::spawn_blocking(|| wait_seeds(std::time::Duration::from_secs(300))).await?;
        crate::prof::note(&mut sp, if done { "done" } else { "gave up after 300s" });
        if t.elapsed().as_secs_f64() > 1.0 {
            eprintln!("keeping the RUN results for the next build on this machine: {:.1}s{}", t.elapsed().as_secs_f64(), if done { "" } else { ", not finished" });
        }
    }
    crate::prof::report(&snap);
    r
}

async fn build_in(o: Opts) -> Result<String> {
    let t0 = Instant::now();
    let text = fs::read_to_string(&o.dockerfile).with_context(|| o.dockerfile.display().to_string())?;
    let (globals, stages) = dockerfile::parse(&text)?;
    let target = dockerfile::target_index(&stages, o.target.as_deref())?;
    let dests: Vec<Ref> = o.dests.iter().map(|d| Ref::parse(d)).collect::<Result<_>>()?;
    let d0 = dests.first().context("--dest is required")?.clone();
    ensure!(dests.iter().all(|d| d.host == d0.host && d.repo == d0.repo), "all --dest must share a repository");
    let cache = match &o.cache {
        Some(c) => Ref::parse(&format!("{c}:x"))?,
        // One cache per registry project: images share base layers and steps.
        None => Ref { host: d0.host.clone(), repo: "internal-services/imgbuild-cache".into(), reference: String::new() },
    };
    let host = format!("linux/{}", match std::env::consts::ARCH { "aarch64" => "arm64", _ => "amd64" });
    let args = o.build_args.iter().map(|a| a.split_once('=').map(|(k, v)| (k.to_string(), v.to_string())).context("--build-arg K=V")).collect::<Result<_>>()?;
    let b = Arc::new(B {
        ignore: Ignore::load(&o.context, &o.dockerfile),
        o, globals, stages, args, host, cache,
        regs: Default::default(), plans: Default::default(), states: Default::default(), images: Default::default(), tops: Default::default(),
        published: Default::default(), rootfs: Default::default(), uploads: Default::default(), live: crate::live::Live::from_env(&d0.host), dest: d0.clone(), target, dirs: Default::default(), blobs: Default::default(), docker: OnceCell::new(),
        early_run: (std::env::var("IMGBUILD_EARLY_RUN").as_deref() == Ok("1")).then(|| Arc::new(tokio::sync::Semaphore::new(2))),
        binfmt: Default::default(), t0,
    });
    let platforms = b.o.platforms.clone();
    // Resolve every image the Dockerfile names at once; planning would otherwise fetch them one by one.
    for p in &platforms {
        let g = b.global_vars(p);
        for st in &b.stages {
            let sp = st.platform.as_deref().map(|x| expand(x, &g)).unwrap_or_else(|| p.clone());
            let mut refs = vec![(expand(&st.from, &g), sp)];
            for ins in st.instrs.iter().filter(|i| i.cmd == "COPY" || i.cmd == "ADD") {
                if let Some((_, f)) = dockerfile::copy_flags(&ins.rest).0.into_iter().find(|(k, _)| k == "from") {
                    refs.push((expand(&f, &g), p.clone()));
                }
            }
            for (r, rp) in refs {
                if r != "scratch" && dockerfile::find_stage(&b.stages, &r).is_none() {
                    let b = b.clone();
                    tokio::spawn(async move { b.image(&r, &rp).await.ok() });
                }
            }
        }
    }
    // Waiting for an image (IMGBUILD_WAIT_FOR_IMAGES) holds up planning the stages that use it: start the
    // others the target needs now, so they build meanwhile.
    // ponytail: starts them even when the target turns out cached; only with the wait set.
    if std::env::var("IMGBUILD_WAIT_FOR_IMAGES").is_ok_and(|v| v != "0") {
        for p in &platforms {
            let g = b.global_vars(p);
            let (mut seen, mut todo) = (vec![false; b.stages.len()], vec![target]);
            while let Some(i) = todo.pop() {
                if std::mem::replace(&mut seen[i], true) {
                    continue;
                }
                let st = &b.stages[i];
                let mut refs = vec![expand(&st.from, &g)];
                for ins in st.instrs.iter().filter(|i| i.cmd == "COPY" || i.cmd == "ADD") {
                    if let Some((_, f)) = dockerfile::copy_flags(&ins.rest).0.into_iter().find(|(k, _)| k == "from") {
                        refs.push(expand(&f, &g));
                    }
                }
                todo.extend(refs.iter().filter_map(|r| dockerfile::find_stage(&b.stages, r)));
            }
            for j in (0..b.stages.len()).filter(|&j| seen[j] && j != target) {
                let (b, p) = (b.clone(), p.clone());
                tokio::spawn(async move { b.state(j, &p).await.ok() });
            }
        }
    }
    let states = try_join_all(platforms.iter().map(|p| b.state(target, p))).await?;
    if !b.o.export.is_empty() {
        for (platform, state) in platforms.iter().zip(&states) {
            if arch_of(platform) != "amd64" {
                continue;
            }
            for (prefix, dest) in &b.o.export {
                let t = Instant::now();
                export_tree(&b, state, prefix, dest).await?;
                eprintln!("export {prefix} -> {} [{:.2}s]", dest.display(), t.elapsed().as_secs_f64());
            }
        }
    }
    let push_id = STEP_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    let push_t = Instant::now();
    let _export = crate::prof::span("export", || format!("#{push_id}"));
    eprintln!("#{push_id} exporting to image");
    // Built in parts (one run per platform, e.g. one CI job per CPU): each part is pushed as `<tag>-<arch>`,
    // and the run that finds every part present pushes the index. Two runs finishing at once both push it;
    // the bytes are the same, so the second PUT is a no-op (or refused as an existing immutable tag).
    // A part tag that already exists is kept, and the index lists what it holds (push_image).
    let partial = !b.o.all_platforms.is_empty();
    let part_tag = |p: &str| format!("{}-{}", d0.reference, arch_of(p));
    let mut built: HashMap<String, Bytes> = try_join_all(platforms.iter().zip(&states).map(|(p, s)| {
        let (b, d0, tag) = (b.clone(), d0.clone(), part_tag(p));
        async move {
            let mut cfg = s.cfg.clone();
            cfg["created"] = json!(crate::rfc3339(b.o.epoch));
            b.push_image(&d0.host, &d0.repo, &s.layers, &cfg, partial.then_some(tag.as_str()), None, Some(push_id)).await.map(|(mb, _)| (p.clone(), mb))
        }
    })).await?.into_iter().collect();
    let mut descs = vec![];
    for p in if partial { &b.o.all_platforms } else { &platforms } {
        let reg = b.reg(&d0.host);
        let tag = part_tag(p);
        let mb = match built.remove(p) {
            Some(mb) => Some(mb),
            // Each run pushes its part before looking for the others, so the last to push sees them all: the
            // registry must show a tag once its PUT returned (see tests/join_race.py).
            None => reg.manifest_opt(&d0.repo, &tag).await?,
        };
        let mb = match mb {
            Some(mb) => mb,
            None => {
                eprintln!("#{push_id} {p} not pushed yet: its run pushes the multi-platform tag");
                eprintln!("#{push_id} DONE {:.1}s", push_t.elapsed().as_secs_f64());
                return Ok(String::new());
            }
        };
        let (os, arch) = p.split_once('/').context("platform")?;
        descs.push(json!({"mediaType": "application/vnd.oci.image.manifest.v1+json", "digest": sha(&mb), "size": mb.len(), "platform": {"os": os, "architecture": arch}}));
    }
    // A Docker manifest list, as buildx pushes: Harbor answers a GET that accepts only Docker manifests (the
    // platform's should-build check) with 404 for an OCI index, which makes every image build again.
    let list_type = "application/vnd.docker.distribution.manifest.list.v2+json";
    let index = Bytes::from(serde_json::to_vec(&json!({"schemaVersion": 2, "mediaType": list_type, "manifests": descs}))?);
    let reg = b.reg(&d0.host);
    for d in &dests {
        let t = Instant::now();
        let _s = crate::prof::span("push-list", || d.reference.clone());
        match reg.put_manifest(&d.repo, &d.reference, list_type, index.clone()).await {
            Ok(_) => eprintln!("#{push_id} pushing manifest for {}/{}:{} {:.1}s done", d.host, d.repo, d.reference, t.elapsed().as_secs_f64()),
            Err(e) => {
                // Tags are immutable in Harbor: an existing one is refused.
                ensure!(reg.manifest_exists(&d.repo, &d.reference).await?, e);
                eprintln!("#{push_id} tag {} already exists, kept", d.reference);
            }
        }
    }
    eprintln!("#{push_id} DONE {:.1}s", push_t.elapsed().as_secs_f64());
    Ok(sha(&index))
}

/// Whether a RUN gives the same result when run again on its own earlier result, so `run_incremental` may.
/// ponytail: an allow-list of commands known to converge (`uv sync` and `pnpm install` make the venv or
/// node_modules match the lock, removing what it no longer lists); add others once checked. IMGBUILD_INCREMENTAL=0 turns it off.
fn incremental(argv: &[String]) -> bool {
    std::env::var("IMGBUILD_INCREMENTAL").map_or(true, |v| v != "0") && argv.last().is_some_and(|c| c.split(&[';', '&', '|'][..]).any(|part| {
        let w: Vec<&str> = part.split_whitespace().filter(|w| !w.contains('=')).collect();
        w.starts_with(&["uv", "sync"]) || w.starts_with(&["pnpm", "install"])
    }))
}

static STEP_ID: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
/// The step each stage label is running, for `docker build --progress=plain`-like output.
static STEPS: std::sync::Mutex<BTreeMap<String, (usize, Instant)>> = std::sync::Mutex::new(BTreeMap::new());

fn step_open(label: &str, i: usize, n: usize, text: &str) -> usize {
    let id = STEP_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    STEPS.lock().unwrap().insert(label.to_string(), (id, Instant::now()));
    eprintln!("#{id} [{label} {}/{n}] {text}", i + 1);
    id
}

fn step_done(label: &str, note: &str) {
    if let Some((id, t)) = STEPS.lock().unwrap().remove(label) {
        eprintln!("#{id} DONE {:.1}s{note}", t.elapsed().as_secs_f64());
        crate::prof::record("step", || format!("#{id} [{label}]{note}"), t);
    }
}

fn step_cached(label: &str, i: usize, n: usize, text: &str) {
    let id = step_open(label, i, n, text);
    STEPS.lock().unwrap().remove(label);
    eprintln!("#{id} CACHED");
}

/// Runs `c`, printing its output under `label`.
fn stream(mut c: Command, label: &str) -> Result<std::process::ExitStatus> {
    let mut c = c.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    let pipes: Vec<Box<dyn Read + Send>> = vec![Box::new(c.stdout.take().unwrap()), Box::new(c.stderr.take().unwrap())];
    let ts: Vec<_> = pipes.into_iter().map(|p| {
        let label = label.to_string();
        std::thread::spawn(move || {
            for line in io::BufReader::new(p).lines().map_while(|l| l.ok()) {
                match STEPS.lock().unwrap().get(&label) {
                    Some((id, t)) => eprintln!("#{id} {:.3} {line}", t.elapsed().as_secs_f64()),
                    None => eprintln!("  [{label}] {line}"),
                }
            }
        })
    }).collect();
    let status = c.wait()?;
    for t in ts {
        let _ = t.join();
    }
    Ok(status)
}

/// Runs a shell script of docker commands in the background, outliving this process: cleanup that the
/// build does not wait for.
/// Seed snapshots (`docker commit`, then `save_seed`'s rename) started in the background; `build` waits for
/// them before it returns. A CI runner pod is deleted when its job ends, which killed them: after a whole
/// RUN the seed was then never kept, and the next build ran the whole RUN again.
static SEEDING: Mutex<Vec<std::process::Child>> = Mutex::new(Vec::new());

static RECORDING: Mutex<Vec<tokio::task::JoinHandle<Result<()>>>> = Mutex::new(Vec::new());
static MEMOS: Mutex<Vec<tokio::task::JoinHandle<Result<()>>>> = Mutex::new(Vec::new());

async fn drain_memos() {
    let hs = std::mem::take(&mut *MEMOS.lock().unwrap());
    let _ = tokio::time::timeout(std::time::Duration::from_secs(1), async {
        for h in hs {
            let _ = h.await;
        }
    }).await;
}

async fn drain_records() -> Result<()> {
    let hs = std::mem::take(&mut *RECORDING.lock().unwrap());
    let mut out = Ok(());
    for h in hs {
        if let Err(e) = h.await? {
            out = Err(e);
        }
    }
    out
}

/// Unlinking a few hundred MB on a busy disk takes seconds, so the directory is renamed (instant) and
/// deleted by a detached `rm`. The pod usually ends first and kills it, so the next job's `prune_node_cache`
/// removes every `del-` directory.
fn remove_scratch(dir: &Path) {
    use std::os::unix::process::CommandExt;
    let gone = dir.with_file_name(format!("del-{}", dir.file_name().unwrap_or_default().to_string_lossy()));
    if fs::rename(dir, &gone).is_ok() {
        let _ = Command::new("rm").arg("-rf").arg(&gone).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).process_group(0).spawn();
    } else {
        let _ = fs::remove_dir_all(dir);
    }
}

fn keep_seed(daemon: Option<&str>, script: &str) {
    if let Some(c) = spawn_detached(daemon, script) {
        SEEDING.lock().unwrap().push(c);
    }
}

/// Waits up to `limit` for the seed snapshots; true when they all finished.
fn wait_seeds(limit: std::time::Duration) -> bool {
    let t = Instant::now();
    let mut v = std::mem::take(&mut *SEEDING.lock().unwrap());
    while t.elapsed() < limit {
        v.retain_mut(|c| matches!(c.try_wait(), Ok(None)));
        if v.is_empty() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    false
}

fn detach(daemon: Option<&str>, script: &str) {
    let _ = spawn_detached(daemon, script);
}

fn spawn_detached(daemon: Option<&str>, script: &str) -> Option<std::process::Child> {
    use std::os::unix::process::CommandExt;
    let mut c = Command::new("sh");
    c.args(["-c", script]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).process_group(0);
    if let Some(h) = daemon {
        c.env("DOCKER_HOST", h);
    }
    c.spawn().ok()
}

/// Records docker image `image` as the result of RUN `lineage` on this machine, whose state is the local
/// step `step`, of which the first `pre` layers are from before the RUN; `depth`: how many incremental runs
/// are stacked on a whole one. Returns a shell command that makes it the seed, to run once the image exists:
/// until then the previous seed stays in use.
fn save_seed(lineage: &str, image: &str, step: &str, pre: usize, depth: u64, daemon: Option<&str>) -> String {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let Some(d) = node_cache("seeds") else { return "true".into() };
    used_image(daemon, image);
    let t = d.join(format!(".pending-{}-{}-{lineage}", uniq(), N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
    let ok = fs::create_dir_all(&d).and_then(|_| fs::write(&t, json!({"image": image, "step": step, "pre": pre, "depth": depth}).to_string()));
    match ok {
        Ok(()) => {
            let f = sh_quote(&d.join(lineage).to_string_lossy());
            let t = sh_quote(&t.to_string_lossy());
            format!("mv {t} {f}")
        }
        Err(_) => "true".into(),
    }
}

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// A directory under IMGBUILD_CACHE_DIR, which outlives the run (the runner's node-local cache).
fn node_cache(name: &str) -> Option<PathBuf> {
    std::env::var_os("IMGBUILD_CACHE_DIR").map(|d| PathBuf::from(d).join(name))
}

/// This run's built layers and extracted filesystems: in the node cache's directory when there is one, as
/// `record` and the subset cache hard-link from them, and link() fails across mount points (a runner's work
/// dir and cache are two mounts of one disk), which made every layer be written twice.
fn scratch(work: &Path) -> PathBuf {
    node_cache("tmp").map_or_else(|| work.to_path_buf(), |d| d.join(uniq()))
}

/// A state kept by `record` in the node's cache: `steps/<key>` lists the layers, built ones are in `blobs/`.
fn save_local_step(s: &State, key: &str) -> Result<()> {
    let (Some(steps), Some(blobs)) = (node_cache("steps"), node_cache("blobs")) else { return Ok(()) };
    fs::create_dir_all(&steps)?;
    fs::create_dir_all(&blobs)?;
    let mut layers = vec![];
    for b in &s.layers {
        let mut l = json!({"digest": b.digest, "diff_id": b.diff_id, "size": b.size, "media_type": b.media_type, "host": b.host, "repo": b.repo});
        if let Some(p) = &b.local {
            let dst = blobs.join(b.digest.trim_start_matches("sha256:"));
            if !dst.exists() {
                // Unique per call: both platforms of a build may keep the same layer at once.
                static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                let t = blobs.join(format!(".tmp-{}-{}-{}", uniq(), N.fetch_add(1, std::sync::atomic::Ordering::Relaxed), &b.digest[7..19]));
                let link = |from: &Path, to: &Path| -> Result<()> {
                    if let Err(e) = fs::hard_link(from, to) {
                        if std::env::var_os("IMGBUILD_TRACE").is_some() {
                            eprintln!("  trace record: copying {} into the node cache ({e})", from.display());
                        }
                        fs::copy(from, to)?;
                    }
                    Ok(())
                };
                link(p, &t)?;
                if plain_of(p).exists() {
                    let tp = plain_of(&t);
                    link(&plain_of(p), &tp)?;
                    fs::rename(&tp, plain_of(&dst))?;
                }
                fs::rename(&t, &dst)?;
            }
            l["local"] = json!(true);
        }
        if let Some(d) = &b.dirs {
            l["dirs"] = json!(dirs_json(d));
        }
        layers.push(l);
    }
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let t = steps.join(format!(".tmp-{}-{}-{key}", uniq(), N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
    fs::write(&t, serde_json::to_vec(&json!({"key": s.key, "cfg": s.cfg, "layers": layers}))?)?;
    fs::rename(&t, steps.join(key))?;
    Ok(())
}

/// A state `save_local_step` kept, while every built layer of it is still in the node's cache.
fn local_step(key: &str) -> Option<State> {
    let t = Instant::now();
    let r = local_step_inner(key);
    if t.elapsed().as_millis() >= 50 {
        crate::prof::record("local-step", || format!("{} {}", &key[..key.len().min(12)], if r.is_some() { "hit" } else { "miss" }), t);
    }
    r
}

fn local_step_inner(key: &str) -> Option<State> {
    let p = node_cache("steps")?.join(key);
    let v: Value = serde_json::from_slice(&fs::read(&p).ok()?).ok()?;
    let blobs = node_cache("blobs")?;
    let mut layers = vec![];
    for l in v["layers"].as_array()? {
        let s = |k: &str| l[k].as_str().unwrap_or_default().to_string();
        let local = match l["local"].as_bool() {
            Some(true) => {
                let f = blobs.join(s("digest").trim_start_matches("sha256:"));
                if !f.exists() {
                    return None;
                }
                touch(&f);
                Some(f)
            }
            _ => None,
        };
        let dirs = l["dirs"].as_str().and_then(|d| dirs_from(&json!({"annotations": {"imgbuild.dirs": d}})));
        layers.push(Blob { digest: s("digest"), diff_id: s("diff_id"), size: l["size"].as_u64()?, media_type: s("media_type"), host: s("host"), repo: s("repo"), local, dirs });
    }
    touch(&p);
    Some(State { layers, cfg: v["cfg"].clone(), key: v["key"].as_str()?.to_string() })
}

fn touch(p: &Path) {
    let _ = fs::File::options().write(true).open(p).and_then(|f| f.set_modified(std::time::SystemTime::now()));
}

/// Writes a small shared cache file without truncating it in place: truncating a file whose pages are being
/// written back waits for that writeback (seconds on a saturated disk), and every job on the node rewrites the
/// same memo files. Same content: just refreshes the mtime. Other content: a new file renamed over it.
fn put_file(p: &Path, data: &[u8]) -> io::Result<()> {
    timed("put-file", || put_file_inner(p, data))
}

/// Records a "slow-fs" span when a shared-cache operation takes 100 ms or more (a job stalling behind
/// other jobs' journal commits), so the profile names it instead of showing a gap.
fn timed<T>(what: &str, f: impl FnOnce() -> T) -> T {
    let t = Instant::now();
    let r = f();
    if t.elapsed().as_millis() >= 100 {
        crate::prof::record("slow-fs", || what.to_string(), t);
    }
    r
}

/// `put_file` off the build's path: the small memo files are only for later builds, so `build` gives them
/// a second at exit and abandons the rest (a killed write leaves a `.tmp-` file, which prune removes).
fn put_bg(p: PathBuf, data: Vec<u8>) {
    let h = tokio::spawn(async move {
        let _ = tokio::task::spawn_blocking(move || {
            if let Some(d) = p.parent() {
                let _ = fs::create_dir_all(d);
            }
            put_file(&p, &data)
        }).await;
        Ok(())
    });
    MEMOS.lock().unwrap().push(h);
}

fn put_file_inner(p: &Path, data: &[u8]) -> io::Result<()> {
    if fs::read(p).is_ok_and(|old| old == data) {
        touch(p);
        return Ok(());
    }
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let tmp = p.with_extension(format!("tmp-{}-{}", uniq(), N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
    fs::write(&tmp, data)?;
    fs::rename(&tmp, p)
}

/// The docker daemon for RUN steps of this architecture: IMGBUILD_DOCKER_AMD64, IMGBUILD_DOCKER_ARM64
/// (a DOCKER_HOST value, e.g. tcp://arm64-docker:2375). Unset: the local daemon, under emulation
/// (binfmt) when the architecture is not the host's.
fn docker_for(arch: &str) -> Option<String> {
    std::env::var(format!("IMGBUILD_DOCKER_{}", arch.to_uppercase())).ok().filter(|h| !h.is_empty())
}

/// Records that a docker image was used, for prune_node_cache.
/// The state as an image in the daemon, without the registry. `docker load` unpacks the whole archive to
/// disk before it looks at a layer (3.5 GB took 40 s), but accepts a missing file for a layer it has and names
/// the first one it lacks; so the first archive holds no layer files, the second the files from that layer on
/// that are on this machine (built here, or in the node cache). None when the daemon lacks a layer that is
/// not here either; the registry has it.
/// One unpack of this state into the shared dockerd. A miss `docker load`s every parent layer, and
/// runners on one node all miss together, so N jobs write that layer N times.
fn lock_load(key: &str) -> Option<File> {
    let dir = node_cache("locks")?;
    fs::create_dir_all(&dir).ok()?;
    let name: String = key.chars().filter(|c| c.is_ascii_hexdigit()).take(40).collect();
    if name.is_empty() {
        return None;
    }
    let f = File::options().create(true).append(true).open(dir.join(name)).ok()?;
    // SAFETY: `f` owns this fd until it is dropped, which releases the lock.
    let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) };
    (rc == 0).then_some(f)
}

fn load_state(s: &State, daemon: Option<&str>) -> Result<Option<String>> {
    let _lock = lock_load(&s.key);
    let tag = format!("imgbuild-state:{}", &s.key[..s.key.len().min(40)]);
    let docker = || {
        let mut c = Command::new("docker");
        if let Some(h) = daemon {
            c.env("DOCKER_HOST", h);
        }
        c
    };
    if docker().args(["image", "inspect", "--format", "-", &tag]).stdout(Stdio::null()).stderr(Stdio::null()).status()?.success() {
        return Ok(Some(tag));
    }
    // Docker's prune age is the image's creation time. The build's reproducible timestamp is not its cache age.
    let mut cfg = s.cfg.clone();
    cfg["created"] = json!(crate::rfc3339(std::time::UNIX_EPOCH.elapsed()?.as_secs()));
    cfg["config"]["Labels"][docker_cache::LABEL_KEY] = json!("image");
    let blobs = node_cache("blobs");
    let mut from = s.layers.len();
    loop {
        let files: Vec<Option<PathBuf>> = s.layers.iter().enumerate().map(|(i, b)| (i >= from).then(|| b.local.clone()
            .or_else(|| blobs.as_ref().map(|d| d.join(b.digest.trim_start_matches("sha256:"))).filter(|p| p.is_file()))).flatten()).collect();
        let err = match docker_load(daemon, &tag, &cfg, &files)? {
            None => {
                if std::env::var_os("IMGBUILD_TRACE").is_some() {
                    eprintln!("  trace load: {} of {} layer files", files.iter().flatten().count(), files.len());
                }
                return Ok(Some(tag));
            }
            Some(e) => e,
        };
        match err.split("absent/").nth(1).and_then(|r| r.split(".tar").next()).and_then(|n| n.parse::<usize>().ok()) {
            Some(n) if n < from => from = n,
            _ => return Ok(None),
        }
    }
}

/// Loads `cfg` as `tag` with the layer files given: None when loaded, docker's error when it lacks a
/// layer left out.
fn docker_load(daemon: Option<&str>, tag: &str, cfg: &Value, files: &[Option<PathBuf>]) -> Result<Option<String>> {
    let docker = || {
        let mut c = Command::new("docker");
        if let Some(h) = daemon {
            c.env("DOCKER_HOST", h);
        }
        c
    };
    let names: Vec<String> = files.iter().enumerate().map(|(i, f)| format!("{}{i}.tar", if f.is_some() { "" } else { "absent/" })).collect();
    let manifest = serde_json::to_vec(&json!([{"Config": "config.json", "RepoTags": [tag], "Layers": names}]))?;
    let config = serde_json::to_vec(cfg)?;
    let mut child = docker().args(["load", "--quiet"]).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped()).spawn()?;
    let written = (|| -> Result<()> {
        let mut t = tar::Builder::new(child.stdin.take().context("docker load stdin")?);
        for (name, data) in [("manifest.json", &manifest), ("config.json", &config)] {
            let mut hd = tar::Header::new_gnu();
            hd.set_size(data.len() as u64);
            hd.set_mode(0o644);
            t.append_data(&mut hd, name, data.as_slice())?;
        }
        for (f, name) in files.iter().zip(&names) {
            if let Some(p) = f {
                t.append_path_with_name(p, name)?;
            }
        }
        t.into_inner()?;
        Ok(())
    })();
    let out = child.wait_with_output()?;
    if out.status.success() && written.is_ok() {
        return Ok(None);
    }
    let err = String::from_utf8_lossy(&out.stderr);
    // The daemon lacks a layer left out of the archive.
    if err.contains("absent/") {
        return Ok(Some(err.into_owned()));
    }
    bail!("docker load: {} {}", err.trim(), written.err().map(|e| format!("{e:#}")).unwrap_or_default())
}

fn used_image(daemon: Option<&str>, r: &str) {
    if let Some(d) = node_cache("images") {
        let id = format!("{}\n{r}", daemon.unwrap_or(""));
        let _ = fs::create_dir_all(&d).and_then(|_| put_file(&d.join(hex::encode(id)), b""));
    }
}

const KEEP_IMAGES: usize = 200;
const KEEP_BLOB_BYTES: u64 = 60 << 30;
const KEEP_SUBSET_BYTES: u64 = 20 << 30;
const MIN_FREE_BYTES: u64 = 20 << 30;

fn disk_space(dir: &Path) -> Option<(u64, u64)> {
    let c = std::ffi::CString::new(dir.as_os_str().as_encoded_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    (unsafe { libc::statvfs(c.as_ptr(), &mut st) } == 0)
        .then(|| (st.f_bavail as u64 * st.f_frsize as u64, st.f_blocks as u64 * st.f_frsize as u64))
}
const KEEP_STEPS: usize = 20000;

/// Least recently used first beyond the limits: docker images a RUN used, and downloaded blobs.
/// ponytail: one process prunes while another may be about to use the image; `docker run --pull missing`
/// then pulls it again. Limits are fixed; move them to flags if a node's disk calls for it.
fn prune_node_cache() {
    // ponytail: at most once a minute while the disk has room; listing the cache costs ~20 ms a time.
    let mark = node_cache("pruned");
    let recent = mark.as_ref().and_then(|m| fs::metadata(m).ok()?.modified().ok()?.elapsed().ok()).is_some_and(|a| a.as_secs() < 60);
    let space = node_cache("").and_then(|d| disk_space(&d));
    let low_disk = space.is_some_and(|(free, total)| free < total / 5);
    if recent && space.is_some_and(|(free, _)| free > MIN_FREE_BYTES) {
        return;
    }
    let _s = crate::prof::span("prune", String::new);
    if !recent && let Some(m) = &mark {
        let _ = put_file(m, b"");
        docker_cache::prune(None, low_disk);
        let mut daemons = [docker_for("amd64"), docker_for("arm64")];
        if daemons[0] == daemons[1] { daemons[1] = None; }
        for host in daemons.iter().flatten() {
            docker_cache::prune(Some(host), false);
        }
    }
    for d in ["dirs", "copyseeds", ""] {
        for e in node_cache(d).and_then(|d| fs::read_dir(d).ok()).into_iter().flatten().flatten() {
            let old = e.metadata().ok().and_then(|m| m.modified().ok()?.elapsed().ok()).is_some_and(|a| a.as_secs() > 3600);
            if old && e.path().extension().is_some_and(|x| x.to_string_lossy().starts_with("tmp-")) {
                let _ = fs::remove_file(e.path());
            }
        }
    }
    // Scratch directories of runs that were killed before cleaning up.
    for e in node_cache("tmp").and_then(|d| fs::read_dir(d).ok()).into_iter().flatten().flatten() {
        if e.file_name().to_string_lossy().starts_with("del-") || e.metadata().ok().and_then(|m| m.modified().ok()?.elapsed().ok()).is_some_and(|a| a.as_secs() > 6 * 3600) {
            let _ = fs::remove_dir_all(e.path());
        }
    }
    let lru = |name: &str| -> Vec<(std::time::SystemTime, u64, PathBuf)> {
        let mut v: Vec<_> = node_cache(name).and_then(|d| fs::read_dir(d).ok()).into_iter().flatten().flatten()
            .filter_map(|e| { let m = e.metadata().ok()?; Some((m.modified().ok()?, m.len(), e.path())) })
            .filter(|(_, _, p)| p.extension().is_none())
            .map(|(t, n, p)| (t, n + fs::metadata(plain_of(&p)).map_or(0, |m| m.len()), p))
            .collect();
        v.sort();
        v
    };
    let images = lru("images");
    let _i = crate::prof::span("prune-images", || format!("{} of {}", images.len().saturating_sub(KEEP_IMAGES), images.len()));
    for (_, _, p) in images.iter().take(images.len().saturating_sub(KEEP_IMAGES)) {
        let id = p.file_name().and_then(|n| hex::decode(n.to_string_lossy().as_bytes()).ok()).map(|b| String::from_utf8_lossy(&b).into_owned());
        if let Some((daemon, r)) = id.as_deref().and_then(|i| i.split_once('\n'))
            && docker_cache::remove_image((!daemon.is_empty()).then_some(daemon), r)
        {
            let _ = fs::remove_file(p);
        }
    }
    for d in ["steps", "copyseeds"] {
        let steps = lru(d);
        for (_, _, p) in steps.iter().take(steps.len().saturating_sub(KEEP_STEPS)) {
            let _ = fs::remove_file(p);
        }
    }
    for (dir, keep) in [("subsets", KEEP_SUBSET_BYTES), ("blobs", KEEP_BLOB_BYTES)] {
        let files = lru(dir);
        let mut total: u64 = files.iter().map(|b| b.1).sum();
        let free = node_cache(dir).and_then(|d| disk_space(&d)).map_or(u64::MAX, |(free, _)| free);
        let keep = keep.min(total.saturating_sub(MIN_FREE_BYTES.saturating_sub(free)));
        let mut sp = crate::prof::span("prune-files", || format!("{dir}: {:.1} GB kept at most {:.1} GB", total as f64 / 1e9, keep as f64 / 1e9));
        let n0 = total;
        for (_, n, p) in &files {
            if total <= keep {
                break;
            }
            let _ = fs::remove_file(p);
            let _ = fs::remove_file(plain_of(p));
            total -= n;
        }
        crate::prof::note(&mut sp, &format!("removed {:.1} GB", (n0 - total) as f64 / 1e9));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_digest_sees_a_rewrite_with_the_same_size_and_mtime() {
        let d = std::env::temp_dir().join(format!("imgbuild-sums-{}", std::process::id()));
        fs::create_dir_all(&d).unwrap();
        let f = d.join("a");
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        fs::write(&f, "one").unwrap();
        File::options().write(true).open(&f).unwrap().set_modified(old).unwrap();
        let a = context_digest(&f).unwrap();
        assert_eq!(context_digest(&f).unwrap(), a);
        fs::write(&f, "two").unwrap();
        File::options().write(true).open(&f).unwrap().set_modified(old).unwrap();
        assert_ne!(context_digest(&f).unwrap(), a);
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn list_dir_sees_a_new_entry_under_the_same_mtime() {
        let d = std::env::temp_dir().join(format!("imgbuild-dirs-{}", std::process::id()));
        fs::create_dir_all(&d).unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        fs::write(d.join("a"), "").unwrap();
        File::open(&d).unwrap().set_modified(old).unwrap();
        assert_eq!(list_dir(&d).unwrap(), ["a"]);
        fs::write(d.join("b"), "").unwrap();
        File::open(&d).unwrap().set_modified(old).unwrap();
        let mut v = list_dir(&d).unwrap();
        v.sort();
        assert_eq!(v, ["a", "b"]);
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn reads_free_disk_space() {
        assert!(disk_space(Path::new("/tmp")).is_some_and(|(free, total)| free > 0 && free <= total));
    }

    #[tokio::test]
    async fn killed_builder_releases_run_lock() -> Result<()> {
        use std::time::Duration;

        const HOLDER_DIR: &str = "IMGBUILD_TEST_RUN_LOCK_HOLDER_DIR";
        if let Some(dir) = std::env::var_os(HOLDER_DIR) {
            let dir = PathBuf::from(dir);
            let _lock = lock_run(&dir, "key", "holder").await?;
            fs::write(dir.join("ready"), b"")?;
            std::future::pending::<()>().await;
            unreachable!();
        }

        let dir = std::env::temp_dir().join(format!("imgbuild-run-lock-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let mut holder = Command::new(std::env::current_exe()?)
            .args(["--exact", "exec::tests::killed_builder_releases_run_lock"])
            .env(HOLDER_DIR, &dir)
            .stdout(Stdio::null())
            .spawn()?;
        let ready = tokio::time::timeout(Duration::from_secs(10), async {
            while !dir.join("ready").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await;
        let blocked = ready.is_ok() && tokio::time::timeout(
            Duration::from_millis(50), lock_run(&dir, "key", "waiter"),
        ).await.is_err();
        holder.kill()?;
        holder.wait()?;
        ready?;
        assert!(blocked, "a live builder must exclude another builder of the same step");
        tokio::time::timeout(Duration::from_secs(5), lock_run(&dir, "key", "waiter")).await??;
        fs::remove_dir_all(&dir)?;
        Ok(())
    }

    #[test]
    fn one_load_of_a_state_at_a_time() {
        let d = std::env::temp_dir().join(format!("imgbuild-lock-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        // This process's other tests do not read IMGBUILD_CACHE_DIR.
        unsafe { std::env::set_var("IMGBUILD_CACHE_DIR", &d) };
        let inside = std::sync::atomic::AtomicUsize::new(0);
        let max = std::sync::atomic::AtomicUsize::new(0);
        std::thread::scope(|s| {
            for _ in 0..4 {
                s.spawn(|| {
                    let _g = lock_load("abc123def456").unwrap();
                    let n = inside.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                    max.fetch_max(n, std::sync::atomic::Ordering::SeqCst);
                    std::thread::sleep(std::time::Duration::from_millis(40));
                    inside.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                });
            }
        });
        unsafe { std::env::remove_var("IMGBUILD_CACHE_DIR") };
        let _ = fs::remove_dir_all(&d);
        assert_eq!(max.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn expands_like_docker() {
        let v: BTreeMap<String, String> = [("A".to_string(), "1".to_string()), ("E".to_string(), String::new())].into();
        assert_eq!(expand("$A/${A}/${B:-d}/${A:+x}/${E:-e}/\\$A", &v), "1/1/d/x/e/$A");
    }

    #[test]
    fn globs_and_ignores() {
        assert!(fnmatch(b"libstdc++.so.6*", b"libstdc++.so.6.0.33"));
        assert!(fnmatch(b"*-linux-gnu", b"aarch64-linux-gnu"));
        assert!(!fnmatch(b"[!a]*", b"abc"));
        let i = Ignore(vec![(false, vec!["**".into(), "node_modules".into()]), (false, vec![".venv".into()]), (true, vec![".venv".into(), "keep".into()])]);
        assert!(i.excluded("a/b/node_modules/x.js"));
        assert!(i.excluded(".venv/lib"));
        assert!(!i.excluded(".venv/keep"));
        assert!(!i.excluded("src/main.py"));
    }

    #[test]
    fn a_context_copy_keeps_the_source_mtime() {
        let root = std::env::temp_dir().join(format!("imgbuild-mt-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let f = File::create(root.join("build.rs")).unwrap();
        f.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_800_000_000)).unwrap();
        let e = copy_entries(&root, None, None, None, &["build.rs".into()], "/src/", None, None).unwrap();
        assert_eq!(e.iter().find(|e| e.path == "src/build.rs").map(|e| e.meta.mtime), Some(1_800_000_000));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_delta_that_only_restates_the_copy_is_redundant() {
        let root = std::env::temp_dir().join(format!("imgbuild-rd-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let mk = |name: &str, entries: &[(&str, &str, u64)]| -> PathBuf {
            let p = root.join(name);
            let mut b = tar::Builder::new(File::create(&p).unwrap());
            for (path, data, mtime) in entries {
                let mut hd = tar::Header::new_gnu();
                hd.set_mode(0o644);
                hd.set_uid(0);
                hd.set_gid(0);
                hd.set_mtime(*mtime);
                hd.set_size(data.len() as u64);
                b.append_data(&mut hd, path, data.as_bytes()).unwrap();
            }
            b.finish().unwrap();
            p
        };
        let old = vec![mk("o1.tar", &[("v/.lock", "", 1), ("v/a", "A", 1)]), mk("o2.tar", &[("v/a", "B", 2)])];
        assert!(redundant(&mk("d1.tar", &[("v/.lock", "", 9)]), &old).unwrap());
        assert!(redundant(&mk("d2.tar", &[("v/a", "B", 9)]), &old).unwrap());
        assert!(!redundant(&mk("d3.tar", &[("v/a", "A", 9)]), &old).unwrap(), "the newer layer's v/a counts");
        assert!(!redundant(&mk("d4.tar", &[("v/new", "", 9)]), &old).unwrap());
        assert!(!redundant(&mk("d5.tar", &[("v/.wh.a", "", 9)]), &old).unwrap());
        let hidden = vec![old[0].clone(), mk("o3.tar", &[("v/.wh.a", "", 3)])];
        assert!(!redundant(&mk("d6.tar", &[("v/a", "A", 9)]), &hidden).unwrap(), "a whited-out file is not in the copy");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn copy_delta_maps_a_source_change_onto_its_copy() {
        let root = std::env::temp_dir().join(format!("imgbuild-cd-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let mk = |name: &str, entries: &[(&str, Option<&str>)]| -> PathBuf {
            let p = root.join(name);
            let mut b = tar::Builder::new(File::create(&p).unwrap());
            for (path, data) in entries {
                let mut hd = tar::Header::new_gnu();
                hd.set_mode(0o640);
                hd.set_uid(7);
                hd.set_gid(7);
                hd.set_mtime(1);
                match data {
                    Some(d) => {
                        hd.set_size(d.len() as u64);
                        b.append_data(&mut hd, path, d.as_bytes()).unwrap();
                    }
                    None => {
                        hd.set_entry_type(tar::EntryType::Directory);
                        hd.set_size(0);
                        b.append_data(&mut hd, format!("{path}/"), io::empty()).unwrap();
                    }
                }
            }
            b.finish().unwrap();
            p
        };
        let delta = mk("d.tar", &[("app", None), ("app/.venv", None), ("app/.venv/x", Some("X")), ("app/.venv/.wh.old", Some("")), ("app/.venv/sub/.wh..wh..opq", Some("")), ("other/y", Some("y"))]);
        let p = vec!["app/.venv".to_string()];
        let map = |fresh| CopyMap { src: "app/.venv".into(), dest: "srv/venv".into(), chown: Some((1000, 1000)), chmod: None, fresh };
        let out = root.join("out");
        let Delta::Layer(l, dirs) = copy_delta(&delta, &p, Some(&map(true)), &out).unwrap() else { panic!("no layer") };
        let got: Vec<(String, u64)> = tar::Archive::new(open_layer(&l.path).unwrap()).entries().unwrap()
            .map(|e| { let e = e.unwrap(); (norm_path(&e.path().unwrap()), e.header().uid().unwrap()) }).collect();
        assert_eq!(got, vec![("srv/venv/x".into(), 1000), ("srv/venv/.wh.old".into(), 0), ("srv/venv/sub/.wh..wh..opq".into(), 0)]);
        assert!(!dirs.contains_key("srv/venv"), "the destination directory is not the source's (BuildKit)");
        assert!(matches!(copy_delta(&delta, &p, Some(&map(false)), &out).unwrap(), Delta::Unsafe), "whiteouts over a destination that held files before");
        assert!(matches!(copy_delta(&delta, &p, None, &out).unwrap(), Delta::Unsafe), "a glob copy is not mapped");
        assert!(matches!(copy_delta(&delta, &["usr/lib".into()], None, &out).unwrap(), Delta::Untouched));
        let moved = mk("m.tar", &[("app/.wh..venv", Some(""))]);
        assert!(matches!(copy_delta(&moved, &p, Some(&map(true)), &out).unwrap(), Delta::Unsafe), "the source itself removed");
        let link = mk("s.tar", &[("app", Some("not a dir"))]);
        assert!(matches!(copy_delta(&link, &p, Some(&map(true)), &out).unwrap(), Delta::Unsafe), "a file above the source");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn keeps_a_hard_link_whose_target_is_outside_the_copied_paths() {
        let root = std::env::temp_dir().join(format!("imgbuild-links-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let layer = root.join("l.tar");
        let mut b = tar::Builder::new(File::create(&layer).unwrap());
        let mut hd = tar::Header::new_gnu();
        hd.set_size(3);
        hd.set_mode(0o644);
        hd.set_uid(0);
        hd.set_gid(0);
        hd.set_mtime(0);
        b.append_data(&mut hd, "root/.cache/uv/x.py", &b"abc"[..]).unwrap();
        let mut hd = tar::Header::new_gnu();
        hd.set_entry_type(tar::EntryType::Link);
        hd.set_size(0);
        hd.set_mode(0o644);
        hd.set_uid(0);
        hd.set_gid(0);
        hd.set_mtime(0);
        b.append_link(&mut hd, "app/.venv/lib/x.py", "root/.cache/uv/x.py").unwrap();
        b.finish().unwrap();
        drop(b);
        let (_, files) = extract(&root.join("fs"), std::slice::from_ref(&layer), &["app/.venv".into()]).unwrap();
        assert_eq!(files.read("app/.venv/lib/x.py").unwrap(), b"abc");
        let sub = root.join("sub.tar");
        layer_subset(&layer, &["app/.venv".into()], &sub).unwrap();
        let (_, files) = extract(&root.join("fs2"), &[sub], &[]).unwrap();
        assert_eq!(files.read("app/.venv/lib/x.py").unwrap(), b"abc");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn put_file_keeps_the_inode_when_the_content_is_the_same() {
        use std::os::unix::fs::MetadataExt;
        let root = std::env::temp_dir().join(format!("imgbuild-put-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let f = root.join("memo");
        put_file(&f, b"one").unwrap();
        let ino = fs::metadata(&f).unwrap().ino();
        put_file(&f, b"one").unwrap();
        assert_eq!((fs::read(&f).unwrap(), fs::metadata(&f).unwrap().ino()), (b"one".to_vec(), ino));
        put_file(&f, b"two").unwrap();
        assert_eq!(fs::read(&f).unwrap(), b"two");
        assert_eq!(fs::read_dir(&root).unwrap().count(), 1);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn canonical_layers_ignore_source_order_and_new_mtimes() {
        let root = std::env::temp_dir().join(format!("imgbuild-canon-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let (a, b) = (root.join("a"), root.join("b"));
        fs::write(&a, "one").unwrap();
        fs::write(&b, "two").unwrap();
        let ent = |p: &str, k: Kind, mtime| Entry { path: p.into(), kind: k, meta: Meta { uid: 0, gid: 0, mode: 0o644, mtime } };
        let layer = |mut v: Vec<Entry>, n: &str| {
            canonical(&mut v, 1000);
            write_layer(&v, &root.join(n)).unwrap().digest
        };
        let x = vec![ent("d", Kind::Dir, 5000), ent("d/a", Kind::File(a.clone()), 5000), ent("d/b", Kind::File(b.clone()), 7)];
        let y = vec![ent("d/b", Kind::File(b), 7), ent("d/a", Kind::File(a), 9000), ent("d", Kind::Dir, 1234)];
        assert_eq!(layer(x, "x"), layer(y, "y"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn context_copy_is_newer_than_anything_below_and_reproducible() {
        let root = std::env::temp_dir().join(format!("imgbuild-ctx-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let a = root.join("a");
        fs::write(&a, "one").unwrap();
        let ent = |mtime| Entry { path: "d/a".into(), kind: Kind::File(a.clone()), meta: Meta { uid: 0, gid: 0, mode: 0o644, mtime } };
        let layer = |mtime, n: &str| {
            let mut v = vec![ent(mtime)];
            for_context(&mut v);
            assert_eq!(v[0].meta.mtime, CONTEXT_MTIME);
            canonical(&mut v, CONTEXT_MTIME);
            write_layer(&v, &root.join(n)).unwrap().digest
        };
        assert_eq!(layer(1_700_000_000, "x"), layer(1_800_000_000, "y"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn split_by_package_keeps_a_packages_files_together_and_is_stable() {
        let ent = |p: &str, dir: bool| Entry { path: p.into(), kind: if dir { Kind::Dir } else { Kind::Link(PathBuf::from("x")) }, meta: Meta { uid: 1, gid: 1, mode: 0o755, mtime: 0 } };
        let sp = "app/.venv/lib/python3.14/site-packages";
        let mk = |pk: &[&str]| {
            let mut v = vec![ent("app", true), ent("app/.venv", true), ent("app/.venv/lib", true), ent("app/.venv/lib/python3.14", true), ent(sp, true), ent("app/.venv/bin", true), ent("app/.venv/bin/python", false)];
            for p in pk {
                v.push(ent(&format!("{sp}/{p}"), true));
                v.push(ent(&format!("{sp}/{p}/__init__.py"), false));
                v.push(ent(&format!("{sp}/{p}-1.0.dist-info"), true));
                v.push(ent(&format!("{sp}/{p}-1.0.dist-info/METADATA"), false));
            }
            v
        };
        assert_eq!(pkg_key("a/site-packages/Foo_Bar-1.2.3.dist-info/RECORD").as_deref(), Some("foo_bar"));
        assert_eq!(pkg_key("a/site-packages/numpy.libs/libx.so").as_deref(), Some("numpy"));
        assert_eq!(pkg_key("a/site-packages/_cffi_backend.cpython-314-x86_64-linux-gnu.so").as_deref(), Some("_cffi_backend"));
        assert_eq!(pkg_key("a/mysite-packages/x"), None);
        let paths = |g: &Vec<Entry>| { let mut p: Vec<String> = g.iter().map(|e| e.path.clone()).collect(); p.sort(); p };
        let before = split_by_package(mk(&["aaa", "bbb", "ccc", "ddd"]), 8);
        let after = split_by_package(mk(&["aaa", "bbb", "ccc", "ddd", "eee"]), 8);
        let all_files: usize = before.iter().flatten().filter(|e| !matches!(e.kind, Kind::Dir)).count();
        assert_eq!(all_files, 9, "every file lands in exactly one layer");
        for g in &before {
            let have: std::collections::HashSet<&str> = g.iter().map(|e| e.path.as_str()).collect();
            assert!(g.iter().all(|e| e.path.rsplit_once('/').is_none_or(|(d, _)| have.contains(d))), "each layer carries the directories above its files");
        }
        let unchanged = before.iter().filter(|g| after.iter().any(|a| paths(a) == paths(g))).count();
        assert!(unchanged >= before.len() - 1, "adding one package changes at most its own layer: {unchanged} of {}", before.len());
        assert_eq!(split_by_package(mk(&["aaa"]), 1).len(), 1);
    }

    #[test]
    fn stamp_pyc_sets_the_recorded_source_mtime_of_timestamp_pycs_only() {
        let mut ts = vec![0u8; 20];
        ts[8..12].copy_from_slice(&1794765322u32.to_le_bytes());
        let mut hashed = ts.clone();
        hashed[4] = 1;
        let before = hashed.clone();
        stamp_pyc(&mut ts, 1704067200);
        stamp_pyc(&mut hashed, 1704067200);
        assert_eq!(u32::from_le_bytes(ts[8..12].try_into().unwrap()), 1704067200);
        assert_eq!(hashed, before, "hash-checked pycs have no mtime");
        stamp_pyc(&mut [0u8; 4], 1);
    }

    #[test]
    fn pycs_land_in_other_buckets_than_their_sources_for_some_packages() {
        let ent = |p: &str| Entry { path: p.into(), kind: Kind::Link(PathBuf::from("x")), meta: Meta { uid: 1, gid: 1, mode: 0o644, mtime: 0 } };
        let sp = "a/site-packages";
        let mut v = vec![];
        for k in 0..40 {
            v.push(ent(&format!("{sp}/p{k}/m.py")));
            v.push(ent(&format!("{sp}/p{k}/__pycache__/m.cpython-314.pyc")));
        }
        let groups = split_by_package(v, 16);
        let together = groups.iter().filter(|g| g.iter().any(|e| e.path.ends_with(".py")) && g.iter().any(|e| e.path.ends_with(".pyc"))).count();
        assert!(together < groups.len(), "pycs are spread independently of sources");
        assert_eq!(groups.iter().map(|g| g.len()).sum::<usize>(), 80 + groups.iter().map(|g| g.iter().filter(|e| matches!(e.kind, Kind::Dir)).count()).sum::<usize>());
    }

    #[test]
    fn extracts_layers_with_whiteouts_and_copies_by_docker_rules() {
        let root = std::env::temp_dir().join(format!("imgbuild-exec-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let mk = |name: &str, entries: &[(&str, Option<&str>, Option<&str>)]| -> PathBuf {
            let p = root.join(name);
            fs::create_dir_all(&root).unwrap();
            let mut b = tar::Builder::new(File::create(&p).unwrap());
            for (path, data, link) in entries {
                let mut hd = tar::Header::new_gnu();
                hd.set_uid(7);
                hd.set_gid(7);
                hd.set_mode(0o640);
                match (data, link) {
                    (_, Some(l)) => {
                        hd.set_entry_type(tar::EntryType::Symlink);
                        hd.set_size(0);
                        b.append_link(&mut hd, path, l).unwrap();
                    }
                    (Some(d), None) => {
                        hd.set_size(d.len() as u64);
                        b.append_data(&mut hd, path, d.as_bytes()).unwrap();
                    }
                    (None, None) => {
                        hd.set_entry_type(tar::EntryType::Directory);
                        hd.set_mode(0o755);
                        hd.set_size(0);
                        b.append_data(&mut hd, format!("{path}/"), io::empty()).unwrap();
                    }
                }
            }
            b.finish().unwrap();
            p
        };
        let l1 = mk("l1.tar", &[("usr", None, None), ("usr/lib", None, None), ("lib", None, Some("usr/lib")), ("usr/lib/a.so.1", Some("A"), None), ("usr/lib/gone", Some("x"), None), ("etc/x", Some("e"), None)]);
        let l2 = mk("l2.tar", &[("usr/lib/.wh.gone", Some(""), None), ("lib/b.so.1", Some("B"), None)]);
        let scanned = scan_dirs(&l1).unwrap();
        assert_eq!(scanned.keys().collect::<Vec<_>>(), vec!["usr", "usr/lib"]);
        let d: Dirs = Arc::new(scanned);
        let back = dirs_from(&json!({"annotations": {"imgbuild.dirs": dirs_json(&d)}})).unwrap();
        assert_eq!((back["usr/lib"].mode, back["usr/lib"].uid), (0o755, 7));
        let fs_dir = root.join("fs");
        let (meta, files) = extract(&fs_dir, &[l1, l2], &["usr".into(), "lib".into()]).unwrap();
        assert!(!files.at.contains_key("usr/lib/gone"), "whiteout applied");
        assert!(!files.at.contains_key("etc/x"), "filtered by prefix");
        assert_eq!(files.read("usr/lib/b.so.1").unwrap(), b"B", "written through the /lib symlink");
        assert_eq!(meta["usr/lib/a.so.1"].uid, 7);
        let e = copy_entries(&fs_dir, Some(&files), Some(&meta), None, &["/lib/*.so.1".into()], "/opt/libs/", Some((1, 2)), None).unwrap();
        let paths: Vec<&str> = e.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, vec!["opt/libs/a.so.1", "opt/libs/b.so.1"]);
        assert_eq!((e[0].meta.uid, e[0].meta.gid, e[0].meta.mode), (1, 2, 0o640));
        let d = copy_entries(&fs_dir, Some(&files), Some(&meta), None, &["/usr/lib".into()], "/x", None, None).unwrap();
        assert_eq!(d.iter().map(|e| e.path.as_str()).collect::<Vec<_>>(), vec!["x/a.so.1", "x/b.so.1"], "the destination directory is not the source's (BuildKit)");
        let l = write_layer(&d, &root.join("out")).unwrap();
        assert_eq!(l.digest, write_layer(&d, &root.join("out2")).unwrap().digest, "layers are reproducible");
        fs::remove_dir_all(&root).unwrap();
    }
}
