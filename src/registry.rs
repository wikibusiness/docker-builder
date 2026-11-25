use anyhow::{Context, Result, bail};
use base64::Engine;
use bytes::Bytes;
use reqwest::{Client, RequestBuilder, Response, StatusCode};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

pub const ACCEPT: &str = "application/vnd.oci.image.index.v1+json, application/vnd.docker.distribution.manifest.list.v2+json, application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json";

#[derive(Clone, Debug)]
pub struct Ref {
    pub host: String,
    pub repo: String,
    pub reference: String,
}

impl Ref {
    /// Parses `host/repo[:tag][@digest]`; a ref without a registry host is a Docker Hub image.
    pub fn parse(s: &str) -> Result<Ref> {
        let (host, rest) = match s.split_once('/') {
            Some((h, r)) if h.contains(['.', ':']) || h == "localhost" => (h.to_string(), r.to_string()),
            Some(_) => ("docker.io".into(), s.to_string()),
            None => ("docker.io".into(), format!("library/{s}")),
        };
        let host = if host == "docker.io" { "registry-1.docker.io".to_string() } else { host };
        let (name, reference) = match rest.split_once('@') {
            Some((n, d)) => (n.to_string(), d.to_string()),
            None => match rest.rsplit_once(':').filter(|(_, t)| !t.contains('/')) {
                Some((n, t)) => (n.to_string(), t.to_string()),
                None => (rest.clone(), "latest".to_string()),
            },
        };
        let repo = match name.rsplit_once(':').filter(|(_, t)| !t.contains('/')) {
            Some((r, _)) if rest.contains('@') => r.to_string(),
            _ => name,
        };
        anyhow::ensure!(!repo.is_empty() && !reference.is_empty(), "bad image reference {s}");
        Ok(Ref { host, repo, reference })
    }
}

#[derive(Clone)]
pub struct Reg {
    pub host: String,
    base: String,
    c: Client,
    creds: Option<(String, String)>,
    tokens: Arc<Mutex<HashMap<String, String>>>,
}

pub(crate) fn creds_for(host: &str) -> Option<(String, String)> {
    if let (Ok(u), Ok(p)) = (std::env::var("IMGBUILD_USER"), std::env::var("IMGBUILD_PASS")) {
        return Some((u, p));
    }
    let home = std::env::var("DOCKER_CONFIG").unwrap_or_else(|_| format!("{}/.docker", std::env::var("HOME").unwrap_or_default()));
    let cfg: serde_json::Value = serde_json::from_slice(&std::fs::read(format!("{home}/config.json")).ok()?).ok()?;
    let key = if host == "registry-1.docker.io" { "https://index.docker.io/v1/" } else { host };
    let auth = cfg["auths"][key]["auth"].as_str()?;
    let dec = String::from_utf8(base64::engine::general_purpose::STANDARD.decode(auth).ok()?).ok()?;
    let (u, p) = dec.split_once(':')?;
    Some((u.into(), p.into()))
}

/// Where content fetched by digest is kept between runs (`IMGBUILD_CACHE_DIR`); it never changes.
fn disk(digest: &str) -> Option<std::path::PathBuf> {
    let hex = digest.strip_prefix("sha256:").filter(|h| h.len() == 64 && h.bytes().all(|c| c.is_ascii_hexdigit()))?;
    Some(Path::new(&std::env::var_os("IMGBUILD_CACHE_DIR")?).join(hex))
}

fn disk_get(digest: &str) -> Option<Bytes> {
    std::fs::read(disk(digest)?).ok().map(Bytes::from)
}

/// A manifest or config the local docker daemon already has (see `exec::local_content`), kept on disk too.
async fn local_get(digest: &str) -> Option<Bytes> {
    disk(digest)?;
    let d = digest.to_string();
    let b = tokio::task::spawn_blocking(move || crate::exec::local_content(&d)).await.ok()??;
    (format!("sha256:{}", hex::encode(sha2::Sha256::digest(&b))) == digest).then(|| {
        disk_put(digest, &b);
        b
    })
}

fn disk_put(digest: &str, body: &Bytes) {
    let Some(p) = disk(digest) else { return };
    if format!("sha256:{}", hex::encode(sha2::Sha256::digest(body))) != digest {
        return;
    }
    let tmp = p.with_extension(format!("part{}", crate::exec::uniq()));
    let _ = std::fs::create_dir_all(p.parent().unwrap()).and_then(|_| std::fs::write(&tmp, body)).and_then(|_| std::fs::rename(&tmp, &p));
}

/// A registry path for the profile: without `/v2/`, digests and upload ids cut to 12 characters.
fn short_path(p: &str) -> String {
    p.trim_start_matches("/v2/").split('/').map(|c| {
        let c = c.strip_prefix("sha256:").unwrap_or(c);
        if c.len() > 30 { &c[..12] } else { c }
    }).collect::<Vec<_>>().join("/")
}

impl Reg {
    pub fn new(host: &str) -> Reg {
        let local = host.starts_with("localhost") || host.starts_with("127.0.0.1");
        Reg {
            host: host.into(),
            base: format!("{}://{host}", if local { "http" } else { "https" }),
            c: Client::builder().pool_max_idle_per_host(32).build().unwrap(),
            creds: creds_for(host),
            tokens: Default::default(),
        }
    }

    async fn send(&self, repo: &str, mk: impl Fn() -> RequestBuilder) -> Result<Response> {
        if crate::prof::on() {
            let mut sp = crate::prof::span("http", || {
                let req = mk().build().ok();
                let path = req.as_ref().map(|r| r.url().path().to_string()).unwrap_or_default();
                format!("{} {}", req.as_ref().map(|r| r.method().as_str()).unwrap_or("?"), short_path(&path))
            });
            let r = self.send_inner(repo, &mk).await;
            crate::prof::note(&mut sp, r.as_ref().map(|r| r.status().as_str().to_string()).unwrap_or_else(|_| "error".into()).as_str());
            return r;
        }
        if std::env::var_os("IMGBUILD_TRACE").is_none() {
            return self.send_inner(repo, &mk).await;
        }
        let t = std::time::Instant::now();
        let r = self.send_inner(repo, &mk).await;
        let req = mk().build().ok();
        eprintln!("  trace {:>5}ms {} {}", t.elapsed().as_millis(), req.as_ref().map(|r| r.method().as_str()).unwrap_or("?"),
            req.as_ref().map(|r| r.url().path().to_string()).unwrap_or_default().chars().take(110).collect::<String>());
        r
    }

    async fn send_inner(&self, repo: &str, mk: &impl Fn() -> RequestBuilder) -> Result<Response> {
        let auth = |rb: RequestBuilder| {
            if let Some(t) = self.tokens.lock().unwrap().get(repo) {
                rb.bearer_auth(t)
            } else if let Some((u, p)) = &self.creds {
                rb.basic_auth(u, Some(p))
            } else {
                rb
            }
        };
        let r = auth(mk()).send().await?;
        if r.status() != StatusCode::UNAUTHORIZED {
            return Ok(r);
        }
        let ch = r.headers().get("www-authenticate").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
        let Some(rest) = ch.strip_prefix("Bearer ") else { return Ok(r) };
        let params = challenge_params(rest);
        let get = |k: &str| params.iter().find(|(n, _)| n.eq_ignore_ascii_case(k)).map(|(_, v)| v.clone());
        let realm = get("realm").context("no realm")?;
        let mut q = vec![];
        if let Some(s) = get("service") { q.push(("service", s)); }
        // ghcr.io names a placeholder scope (repository:user/image:pull) whatever repo was asked for.
        match get("scope") {
            Some(s) if s.contains(&format!("repository:{repo}:")) => q.push(("scope", s)),
            _ => q.push(("scope", format!("repository:{repo}:pull"))),
        }
        let mut rb = self.c.get(&realm).query(&q);
        if let Some((u, p)) = &self.creds { rb = rb.basic_auth(u, Some(p)); }
        let mut resp = rb.send().await?;
        if !resp.status().is_success() && self.creds.is_some() {
            // IMGBUILD_USER is meant for our registry; public registries reject it, but serve anonymous tokens.
            resp = self.c.get(&realm).query(&q).send().await?;
        }
        let tok: serde_json::Value = resp.error_for_status()?.json().await?;
        let t = tok["token"].as_str().or(tok["access_token"].as_str()).context("no token")?.to_string();
        self.tokens.lock().unwrap().insert(repo.into(), t.clone());
        Ok(mk().bearer_auth(t).send().await?)
    }

    pub async fn manifest(&self, repo: &str, reference: &str) -> Result<(Bytes, String, String)> {
        let ct_of = |body: &Bytes| serde_json::from_slice::<serde_json::Value>(body).ok().and_then(|v| v["mediaType"].as_str().map(String::from)).unwrap_or_default();
        if let Some(body) = disk_get(reference) {
            return Ok((body.clone(), ct_of(&body), reference.to_string()));
        }
        if let Some(body) = local_get(reference).await {
            return Ok((body.clone(), ct_of(&body), reference.to_string()));
        }
        let tag = disk(&format!("sha256:{}", hex::encode(sha2::Sha256::digest(format!("tag {}/{repo}:{reference}", self.base)))))
            .filter(|_| !reference.starts_with("sha256:"));
        if let Some(t) = &tag {
            // ponytail: like `docker build` without --pull, a tag resolves to what this machine last saw; unlike
            // docker, only for IMGBUILD_TAG_TTL seconds (default an hour), so moving tags still update.
            let ttl = std::env::var("IMGBUILD_TAG_TTL").ok().and_then(|v| v.parse().ok()).unwrap_or(3600);
            let fresh = std::fs::metadata(t).and_then(|m| m.modified()).ok().and_then(|m| m.elapsed().ok()).is_some_and(|a| a.as_secs() < ttl);
            if let Some(d) = fresh.then(|| std::fs::read_to_string(t).ok()).flatten()
                && let Some(body) = disk_get(&d) {
                    return Ok((body.clone(), ct_of(&body), d));
                }
        }
        let url = format!("{}/v2/{repo}/manifests/{reference}", self.base);
        let r = self.send(repo, || self.c.get(&url).header("Accept", ACCEPT)).await?;
        if !r.status().is_success() {
            bail!("GET manifest {repo}:{reference}: {}", r.status());
        }
        let ct = r.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
        let body = r.bytes().await?;
        let digest = format!("sha256:{}", hex::encode(sha2::Sha256::digest(&body)));
        if digest == reference || tag.is_some() {
            disk_put(&digest, &body);
        }
        if let Some(t) = tag {
            let tmp = t.with_extension(format!("part{}", crate::exec::uniq()));
            let _ = std::fs::write(&tmp, &digest).and_then(|_| std::fs::rename(&tmp, &t));
        }
        Ok((body, ct, digest))
    }

    /// The manifest, or None when the registry has no such reference.
    pub async fn manifest_opt(&self, repo: &str, reference: &str) -> Result<Option<Bytes>> {
        let url = format!("{}/v2/{repo}/manifests/{reference}", self.base);
        let r = self.send(repo, || self.c.get(&url).header("Accept", ACCEPT)).await?;
        match r.status() {
            s if s.is_success() => Ok(Some(r.bytes().await?)),
            reqwest::StatusCode::NOT_FOUND => Ok(None),
            s => bail!("GET manifest {repo}:{reference}: {s}"),
        }
    }

    pub async fn manifest_exists(&self, repo: &str, reference: &str) -> Result<bool> {
        let url = format!("{}/v2/{repo}/manifests/{reference}", self.base);
        let r = self.send(repo, || self.c.head(&url).header("Accept", ACCEPT)).await?;
        Ok(r.status().is_success())
    }

    pub async fn put_manifest(&self, repo: &str, reference: &str, media_type: &str, body: Bytes) -> Result<()> {
        let url = format!("{}/v2/{repo}/manifests/{reference}", self.base);
        let r = self.send(repo, || self.c.put(&url).header("Content-Type", media_type).body(body.clone())).await?;
        if !r.status().is_success() {
            bail!("PUT manifest {repo}:{reference}: {} {}", r.status(), r.text().await.unwrap_or_default());
        }
        Ok(())
    }

    pub async fn blob_exists(&self, repo: &str, digest: &str) -> Result<bool> {
        let url = format!("{}/v2/{repo}/blobs/{digest}", self.base);
        Ok(self.send(repo, || self.c.head(&url)).await?.status().is_success())
    }

    pub async fn get_blob(&self, repo: &str, digest: &str) -> Result<Bytes> {
        if let Some(b) = disk_get(digest) {
            return Ok(b);
        }
        if let Some(b) = local_get(digest).await {
            return Ok(b);
        }
        let url = format!("{}/v2/{repo}/blobs/{digest}", self.base);
        let r = self.send(repo, || self.c.get(&url)).await?;
        if !r.status().is_success() {
            bail!("GET blob {digest}: {}", r.status());
        }
        let b = r.bytes().await?;
        disk_put(digest, &b);
        Ok(b)
    }

    /// Streams a blob into `path`, checking its digest.
    pub async fn get_blob_file(&self, repo: &str, digest: &str, path: &Path) -> Result<()> {
        use tokio::io::AsyncWriteExt;
        let (d, p) = (digest.to_string(), path.to_path_buf());
        if tokio::task::spawn_blocking(move || crate::exec::local_content_file(&d, &p)).await.unwrap_or(false) {
            return Ok(());
        }
        let url = format!("{}/v2/{repo}/blobs/{digest}", self.base);
        let mut r = self.send(repo, || self.c.get(&url)).await?;
        if !r.status().is_success() {
            bail!("GET blob {repo}@{digest}: {}", r.status());
        }
        let tmp = path.with_extension(format!("part{}", crate::exec::uniq()));
        let mut f = tokio::io::BufWriter::new(tokio::fs::File::create(&tmp).await?);
        let mut h = sha2::Sha256::new();
        while let Some(c) = r.chunk().await? {
            h.update(&c);
            f.write_all(&c).await?;
        }
        f.flush().await?;
        let got = format!("sha256:{}", hex::encode(h.finalize()));
        anyhow::ensure!(got == digest, "blob {digest} downloaded as {got}");
        tokio::fs::rename(&tmp, path).await?;
        Ok(())
    }

    /// Uploads a file as a blob, in chunks so a multi-GB layer is never held in memory.
    pub async fn put_blob_file(&self, repo: &str, digest: &str, path: &Path) -> Result<()> {
        use tokio::io::AsyncReadExt;
        const CHUNK: usize = 64 << 20;
        let size = tokio::fs::metadata(path).await?.len();
        if size as usize <= CHUNK {
            return self.put_blob(repo, digest, Bytes::from(tokio::fs::read(path).await?)).await;
        }
        let url = format!("{}/v2/{repo}/blobs/uploads/", self.base);
        let r = self.send(repo, || self.c.post(&url).header("Content-Length", "0")).await?;
        if !r.status().is_success() {
            bail!("POST upload {repo}: {}", r.status());
        }
        let mut loc = self.abs(r.headers().get("location").and_then(|v| v.to_str().ok()).context("no location")?);
        let mut f = tokio::fs::File::open(path).await?;
        let mut off = 0u64;
        while off < size {
            let mut buf = vec![0u8; CHUNK.min((size - off) as usize)];
            f.read_exact(&mut buf).await?;
            let (body, end) = (Bytes::from(buf), off + CHUNK.min((size - off) as usize) as u64 - 1);
            let r = self.send(repo, || {
                self.c.patch(&loc).header("Content-Type", "application/octet-stream")
                    .header("Content-Range", format!("{off}-{end}")).body(body.clone())
            }).await?;
            if !r.status().is_success() {
                bail!("PATCH blob {digest}: {} {}", r.status(), r.text().await.unwrap_or_default());
            }
            loc = self.abs(r.headers().get("location").and_then(|v| v.to_str().ok()).context("no location")?);
            off = end + 1;
        }
        let sep = if loc.contains('?') { '&' } else { '?' };
        let put = format!("{loc}{sep}digest={digest}");
        let r = self.send(repo, || self.c.put(&put).header("Content-Length", "0")).await?;
        if !r.status().is_success() {
            bail!("PUT blob {digest}: {} {}", r.status(), r.text().await.unwrap_or_default());
        }
        Ok(())
    }

    fn abs(&self, loc: &str) -> String {
        if loc.starts_with('/') { format!("{}{loc}", self.base) } else { loc.to_string() }
    }

    pub async fn mount(&self, repo: &str, digest: &str, from: &str) -> Result<bool> {
        let url = format!("{}/v2/{repo}/blobs/uploads/?mount={digest}&from={from}", self.base);
        let r = self.send(repo, || self.c.post(&url).header("Content-Length", "0")).await?;
        Ok(r.status() == StatusCode::CREATED)
    }

    pub async fn put_blob(&self, repo: &str, digest: &str, body: Bytes) -> Result<()> {
        let url = format!("{}/v2/{repo}/blobs/uploads/", self.base);
        let r = self.send(repo, || self.c.post(&url).header("Content-Length", "0")).await?;
        if !r.status().is_success() {
            bail!("POST upload {repo}: {}", r.status());
        }
        let loc = self.abs(r.headers().get("location").and_then(|v| v.to_str().ok()).context("no location")?);
        let sep = if loc.contains('?') { '&' } else { '?' };
        let put = format!("{loc}{sep}digest={digest}");
        let r = self.send(repo, || self.c.put(&put).header("Content-Type", "application/octet-stream").body(body.clone())).await?;
        if !r.status().is_success() {
            bail!("PUT blob {digest}: {} {}", r.status(), r.text().await.unwrap_or_default());
        }
        Ok(())
    }
}

use sha2::Digest;

/// The parameters of a `WWW-Authenticate: Bearer` challenge. A quoted value may hold commas:
/// Harbor asks for `scope="repository:example/image:pull,push"`.
fn challenge_params(s: &str) -> Vec<(String, String)> {
    let mut out = vec![];
    let mut it = s.chars().peekable();
    loop {
        while it.next_if(|c| *c == ',' || c.is_whitespace()).is_some() {}
        let key: String = std::iter::from_fn(|| it.next_if(|c| *c != '=' && *c != ',')).collect();
        if key.is_empty() {
            break;
        }
        let mut val = String::new();
        if it.next_if_eq(&'=').is_some() {
            if it.next_if_eq(&'"').is_some() {
                while let Some(c) = it.next() {
                    match c {
                        '\\' => val.extend(it.next()),
                        '"' => break,
                        c => val.push(c),
                    }
                }
            } else {
                val = std::iter::from_fn(|| it.next_if(|c| *c != ',')).collect();
            }
        }
        out.push((key.trim().to_string(), val));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::challenge_params;

    fn get(p: &[(String, String)], k: &str) -> String {
        p.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone()).unwrap_or_default()
    }

    #[test]
    fn bearer_challenge_keeps_commas_inside_quotes() {
        let p = challenge_params(r#"realm="https://h.example/service/token",service="harbor-registry",scope="repository:example/image:pull,push""#);
        assert_eq!(get(&p, "realm"), "https://h.example/service/token");
        assert_eq!(get(&p, "service"), "harbor-registry");
        assert_eq!(get(&p, "scope"), "repository:example/image:pull,push");
        let p = challenge_params(r#"realm="https://ghcr.io/token",service="ghcr.io",scope="repository:user/image:pull""#);
        assert_eq!(get(&p, "scope"), "repository:user/image:pull");
        assert_eq!(challenge_params("").len(), 0);
        assert_eq!(get(&challenge_params(r#"a=b, c="d\"e""#), "c"), "d\"e");
    }
}
