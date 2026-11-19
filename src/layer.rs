use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

pub struct Layer {
    pub path: PathBuf,
    pub digest: String,
    pub diff_id: String,
    pub size: u64,
}

pub struct Hashing<W> {
    pub inner: W,
    pub h: Sha256,
    pub n: u64,
}

impl<W: Write> Write for Hashing<W> {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(b)?;
        self.h.update(&b[..n]);
        self.n += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

pub struct Source<'a> {
    pub key: Option<&'a str>,
    pub src: &'a Path,
    pub dest: &'a str,
    pub uid: u64,
    pub gid: u64,
    pub epoch: u64,
    /// File-name globs left out of the layer.
    pub exclude: &'a [String],
}

impl Source<'_> {
    fn excluded(&self, name: &std::ffi::OsStr) -> bool {
        self.exclude.iter().any(|g| crate::exec::fnmatch(g.as_bytes(), name.as_encoded_bytes()))
    }
}

pub fn build(s: &Source, out_dir: &Path) -> Result<Layer> {
    fs::create_dir_all(out_dir)?;
    let tmp = out_dir.join(format!(".tmp-{}-{:?}", crate::exec::uniq(), std::thread::current().id()));
    let file = Hashing { inner: BufWriter::with_capacity(1 << 20, File::create(&tmp)?), h: Sha256::new(), n: 0 };
    let mut enc = zstd::Encoder::new(file, 3)?;
    enc.multithread(std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(4))?;
    let tar_w = Hashing { inner: enc, h: Sha256::new(), n: 0 };
    let mut b = tar::Builder::new(tar_w);
    b.mode(tar::HeaderMode::Deterministic);

    let dest = s.dest.trim_matches('/');
    let mut seen = BTreeSet::new();
    let comps: Vec<&str> = dest.split('/').collect();
    let meta = fs::symlink_metadata(s.src).with_context(|| format!("stat {}", s.src.display()))?;
    let parents = comps.len() - 1;
    for i in 1..=parents {
        let d = comps[..i].join("/");
        if seen.insert(d.clone()) {
            dir_entry(&mut b, &d, 0, 0, 0o755, s.epoch)?;
        }
    }
    if meta.is_dir() {
        dir_entry(&mut b, dest, s.uid, s.gid, 0o755, s.epoch)?;
        walk(&mut b, s.src, dest, s)?;
    } else {
        file_entry(&mut b, s.src, dest, &meta, s)?;
    }
    let tar_w = b.into_inner()?;
    let diff_id = hex::encode(tar_w.h.clone().finalize());
    let mut file = tar_w.inner.finish()?;
    file.flush()?;
    let digest = hex::encode(file.h.clone().finalize());
    let size = file.n;
    drop(file);
    let path = out_dir.join(&digest);
    fs::rename(&tmp, &path)?;
    Ok(Layer { path, digest: format!("sha256:{digest}"), diff_id: format!("sha256:{diff_id}"), size })
}

fn header(kind: tar::EntryType, uid: u64, gid: u64, mode: u32, epoch: u64, size: u64) -> tar::Header {
    let mut h = tar::Header::new_gnu();
    h.set_entry_type(kind);
    h.set_uid(uid);
    h.set_gid(gid);
    h.set_mode(mode);
    h.set_mtime(epoch);
    h.set_size(size);
    h
}

fn dir_entry<W: Write>(b: &mut tar::Builder<W>, path: &str, uid: u64, gid: u64, mode: u32, epoch: u64) -> Result<()> {
    let mut h = header(tar::EntryType::Directory, uid, gid, mode, epoch, 0);
    b.append_data(&mut h, format!("{path}/"), io::empty())?;
    Ok(())
}

fn file_entry<W: Write>(b: &mut tar::Builder<W>, src: &Path, dest: &str, m: &fs::Metadata, s: &Source) -> Result<()> {
    if m.file_type().is_symlink() {
        let mut h = header(tar::EntryType::Symlink, s.uid, s.gid, 0o777, s.epoch, 0);
        b.append_link(&mut h, dest, fs::read_link(src)?)?;
    } else if m.is_file() {
        let mode = if m.permissions().mode() & 0o111 != 0 { 0o755 } else { 0o644 };
        let mut h = header(tar::EntryType::Regular, s.uid, s.gid, mode, s.epoch, m.len());
        b.append_data(&mut h, dest, File::open(src).with_context(|| src.display().to_string())?)?;
    }
    Ok(())
}

fn walk<W: Write>(b: &mut tar::Builder<W>, dir: &Path, dest: &str, s: &Source) -> Result<()> {
    let mut names: Vec<_> = fs::read_dir(dir)?.collect::<io::Result<Vec<_>>>()?;
    names.sort_by_key(|e| e.file_name());
    for e in names.into_iter().filter(|e| !s.excluded(&e.file_name())) {
        let p = e.path();
        let d = format!("{dest}/{}", e.file_name().to_string_lossy());
        let m = fs::symlink_metadata(&p)?;
        if m.is_dir() {
            dir_entry(b, &d, s.uid, s.gid, 0o755, s.epoch)?;
            walk(b, &p, &d, s)?;
        } else {
            file_entry(b, &p, &d, &m, s)?;
        }
    }
    Ok(())
}

fn collect(dir: &Path, rel: &str, s: &Source, out: &mut Vec<(PathBuf, String)>) -> Result<()> {
    let mut names: Vec<_> = fs::read_dir(dir)?.collect::<io::Result<Vec<_>>>()?;
    names.sort_by_key(|e| e.file_name());
    for e in names.into_iter().filter(|e| !s.excluded(&e.file_name())) {
        let p = e.path();
        let r = format!("{rel}/{}", e.file_name().to_string_lossy());
        if fs::symlink_metadata(&p)?.is_dir() {
            collect(&p, &r, s, out)?;
        } else {
            out.push((p, r));
        }
    }
    Ok(())
}

pub fn file_digest(p: &Path) -> Result<[u8; 32]> {
    let m = fs::symlink_metadata(p)?;
    let mut h = Sha256::new();
    if m.file_type().is_symlink() {
        h.update(b"l");
        h.update(fs::read_link(p)?.as_os_str().as_encoded_bytes());
    } else {
        h.update(if m.permissions().mode() & 0o111 != 0 { b"x" } else { b"f" });
        let mut f = File::open(p)?;
        let mut buf = vec![0u8; 1 << 16];
        loop {
            let n = io::Read::read(&mut f, &mut buf)?;
            if n == 0 { break; }
            h.update(&buf[..n]);
        }
    }
    Ok(h.finalize().into())
}

/// Cache key for a layer: a hash of everything that affects the layer bytes.
/// An explicit `key` (e.g. a lockfile hash) skips reading the tree.
pub fn cache_key(s: &Source) -> Result<String> {
    let mut h = Sha256::new();
    h.update(format!("imgbuild-v1|{}|{}|{}|{}|{}|{:?}|", s.src.display(), s.dest, s.uid, s.gid, s.epoch, s.exclude));
    if let Some(k) = s.key {
        h.update(format!("key:{k}"));
        return Ok(hex::encode(h.finalize()));
    }
    let mut files = vec![];
    if fs::symlink_metadata(s.src)?.is_dir() {
        collect(s.src, "", s, &mut files)?;
    } else {
        files.push((s.src.to_path_buf(), String::new()));
    }
    let n = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    let mut sums = vec![[0u8; 32]; files.len()];
    let chunk = files.len().div_ceil(n).max(1);
    std::thread::scope(|sc| -> Result<()> {
        let hs: Vec<_> = files.chunks(chunk).zip(sums.chunks_mut(chunk)).map(|(f, o)| {
            sc.spawn(move || -> Result<()> {
                for (i, (p, _)) in f.iter().enumerate() { o[i] = file_digest(p)?; }
                Ok(())
            })
        }).collect();
        for h in hs { h.join().map_err(|_| anyhow::anyhow!("hash thread panicked"))??; }
        Ok(())
    })?;
    for ((_, rel), sum) in files.iter().zip(&sums) {
        h.update(rel.as_bytes());
        h.update(sum);
    }
    Ok(hex::encode(h.finalize()))
}

/// Build the layer, or reuse a previous build of identical content.
pub fn build_cached(s: &Source, work: &Path) -> Result<(Layer, bool)> {
    let idx = work.join("keys");
    fs::create_dir_all(&idx)?;
    let key = cache_key(s)?;
    let entry = idx.join(&key);
    if let Ok(v) = fs::read_to_string(&entry) {
        let p: Vec<&str> = v.trim().split(' ').collect();
        if let [digest, diff_id, size] = p[..] {
            let path = work.join(digest.trim_start_matches("sha256:"));
            if path.exists() {
                return Ok((Layer { path, digest: digest.into(), diff_id: diff_id.into(), size: size.parse()? }, true));
            }
        }
    }
    let l = build(s, work)?;
    fs::write(&entry, format!("{} {} {}", l.digest, l.diff_id, l.size))?;
    Ok((l, false))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src<'a>(p: &'a Path) -> Source<'a> {
        Source { key: None, src: p, dest: "usr/app", uid: 1000, gid: 1000, epoch: 0, exclude: &[] }
    }

    #[test]
    fn layers_are_reproducible_and_content_addressed() {
        let root = std::env::temp_dir().join(format!("imgbuild-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("in/sub")).unwrap();
        fs::write(root.join("in/a.txt"), "hello").unwrap();
        fs::write(root.join("in/sub/b.txt"), "world").unwrap();
        let first = build(&src(&root.join("in")), &root.join("out1")).unwrap();
        // Rewriting a file changes its mtime but not the layer.
        fs::write(root.join("in/a.txt"), "hello").unwrap();
        let second = build(&src(&root.join("in")), &root.join("out2")).unwrap();
        assert_eq!(first.digest, second.digest);
        let k1 = cache_key(&src(&root.join("in"))).unwrap();
        fs::write(root.join("in/a.txt"), "changed").unwrap();
        assert_ne!(k1, cache_key(&src(&root.join("in"))).unwrap());
        fs::remove_dir_all(&root).unwrap();
    }
}
