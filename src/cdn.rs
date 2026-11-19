use anyhow::{Context, Result, bail};
use futures::stream::{self, StreamExt};
use hmac::{Hmac, KeyInit, Mac};
use reqwest::Client;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

type HmacSha256 = Hmac<Sha256>;

pub struct S3 {
    c: Client,
    endpoint: String,
    region: String,
    ak: String,
    sk: String,
}

fn hmac(key: &[u8], msg: &str) -> Vec<u8> {
    let mut m = <HmacSha256 as KeyInit>::new_from_slice(key).unwrap();
    m.update(msg.as_bytes());
    m.finalize().into_bytes().to_vec()
}

fn enc(s: &str, keep_slash: bool) -> String {
    let mut o = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => o.push(b as char),
            b'/' if keep_slash => o.push('/'),
            _ => o.push_str(&format!("%{b:02X}")),
        }
    }
    o
}

fn amz_date() -> (String, String) {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    let s = crate::rfc3339(secs); // 2026-11-11T10:00:00Z
    let d = s.replace(['-', ':'], "");
    (d.clone(), d[..8].to_string())
}

pub fn content_type(p: &str) -> &'static str {
    match p.rsplit('.').next().unwrap_or("") {
        "js" | "mjs" => "text/javascript",
        "css" => "text/css",
        "html" => "text/html",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/vnd.microsoft.icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "wasm" => "application/wasm",
        "txt" => "text/plain",
        "xml" => "application/xml",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        _ => "application/octet-stream",
    }
}

impl S3 {
    pub fn from_env(endpoint: &str, region: &str) -> Result<S3> {
        Ok(S3 {
            c: Client::builder().pool_max_idle_per_host(64).timeout(Duration::from_secs(120)).build()?,
            endpoint: endpoint.trim_end_matches('/').to_string(),
            region: region.into(),
            ak: std::env::var("AWS_ACCESS_KEY_ID").context("AWS_ACCESS_KEY_ID")?,
            sk: std::env::var("AWS_SECRET_ACCESS_KEY").context("AWS_SECRET_ACCESS_KEY")?,
        })
    }

    async fn req(&self, method: &str, path: &str, query: &[(&str, String)], extra: &[(&str, String)], body: Vec<u8>) -> Result<reqwest::Response> {
        let host = self.endpoint.split("//").nth(1).unwrap().to_string();
        let (dt, d) = amz_date();
        let payload = hex::encode(Sha256::digest(&body));
        let mut q: Vec<(String, String)> = query.iter().map(|(k, v)| (enc(k, false), enc(v, false))).collect();
        q.sort();
        let cq = q.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&");
        let mut hdrs: Vec<(String, String)> = vec![("host".into(), host), ("x-amz-content-sha256".into(), payload.clone()), ("x-amz-date".into(), dt.clone())];
        for (k, v) in extra { hdrs.push((k.to_lowercase(), v.clone())); }
        hdrs.sort();
        let ch: String = hdrs.iter().map(|(k, v)| format!("{k}:{}\n", v.trim())).collect();
        let sh = hdrs.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(";");
        let cpath = enc(path, true);
        let canon = format!("{method}\n{cpath}\n{cq}\n{ch}\n{sh}\n{payload}");
        let scope = format!("{d}/{}/s3/aws4_request", self.region);
        let sts = format!("AWS4-HMAC-SHA256\n{dt}\n{scope}\n{}", hex::encode(Sha256::digest(canon.as_bytes())));
        let k = hmac(&hmac(&hmac(&hmac(format!("AWS4{}", self.sk).as_bytes(), &d), &self.region), "s3"), "aws4_request");
        let sig = hex::encode(hmac(&k, &sts));
        let auth = format!("AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={sh}, Signature={sig}", self.ak);
        let url = format!("{}{cpath}{}", self.endpoint, if cq.is_empty() { String::new() } else { format!("?{cq}") });
        let mut rb = self.c.request(method.parse()?, url).header("x-amz-date", dt).header("x-amz-content-sha256", payload).header("authorization", auth);
        for (k, v) in extra { rb = rb.header(*k, v); }
        // reqwest omits Content-Length for an empty body, which R2 rejects with 411.
        Ok(rb.header("content-length", body.len()).body(body).send().await?)
    }

    pub async fn put(&self, bucket: &str, key: &str, body: Vec<u8>, ctype: &str, cache: &str) -> Result<()> {
        let mut last = String::new();
        for attempt in 0..6u64 {
            let r = self.req("PUT", &format!("/{bucket}/{key}"), &[], &[("content-type", ctype.into()), ("cache-control", cache.into())], body.clone()).await;
            match r {
                Ok(r) if r.status().is_success() => return Ok(()),
                Ok(r) => {
                    let st = r.status();
                    last = format!("{st} {}", r.text().await.unwrap_or_default());
                    if !(st.is_server_error() || st.as_u16() == 429) { break; }
                }
                Err(e) => last = e.to_string(),
            }
            tokio::time::sleep(Duration::from_millis(400 * (1 << attempt))).await;
        }
        bail!("PUT {key}: {last}")
    }

    /// key -> size for everything under prefix
    pub async fn list(&self, bucket: &str, prefix: &str) -> Result<HashMap<String, u64>> {
        let mut out = HashMap::new();
        let mut token: Option<String> = None;
        loop {
            let mut q = vec![("list-type", "2".to_string()), ("prefix", prefix.to_string())];
            if let Some(t) = &token { q.push(("continuation-token", t.clone())); }
            let r = self.req("GET", &format!("/{bucket}"), &q, &[], vec![]).await?;
            if !r.status().is_success() { bail!("LIST {prefix}: {} {}", r.status(), r.text().await.unwrap_or_default()); }
            let x = r.text().await?;
            for c in x.split("<Contents>").skip(1) {
                let tag = |t: &str| c.split(&format!("<{t}>")).nth(1).and_then(|s| s.split(&format!("</{t}>")).next()).unwrap_or("").to_string();
                out.insert(tag("Key").replace("&amp;", "&"), tag("Size").parse().unwrap_or(0));
            }
            token = x.split("<NextContinuationToken>").nth(1).and_then(|s| s.split("</NextContinuationToken>").next()).map(String::from);
            if !x.contains("<IsTruncated>true</IsTruncated>") || token.is_none() { break; }
        }
        Ok(out)
    }
}

fn walk(dir: &Path, rel: &str, out: &mut Vec<(PathBuf, String)>) -> Result<()> {
    for e in std::fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))? {
        let e = e?;
        let r = if rel.is_empty() { e.file_name().to_string_lossy().into() } else { format!("{rel}/{}", e.file_name().to_string_lossy()) };
        if e.file_type()?.is_dir() { walk(&e.path(), &r, out)?; } else { out.push((e.path(), r)); }
    }
    Ok(())
}

pub struct Args {
    pub dir: PathBuf,
    pub bucket: String,
    pub prefix: String,
    pub marker: Option<String>,
    pub endpoint: String,
    pub region: String,
    pub cache_control: String,
    pub concurrency: usize,
}

/// aws s3 sync --size-only equivalent (excluding *.br *.gz *.map), then the marker object.
pub async fn sync(a: Args) -> Result<()> {
    let s3 = S3::from_env(&a.endpoint, &a.region)?;
    let t = std::time::Instant::now();
    let mut files = vec![];
    walk(&a.dir, "", &mut files)?;
    files.retain(|(_, r)| !(r.ends_with(".br") || r.ends_with(".gz") || r.ends_with(".map")));
    let prefix = a.prefix.trim_matches('/').to_string();
    let existing = s3.list(&a.bucket, &format!("{prefix}/")).await?;
    let todo: Vec<_> = files.iter().filter(|(p, r)| {
        let size = std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
        existing.get(&format!("{prefix}/{r}")) != Some(&size)
    }).collect();
    eprintln!("cdn: {} files, {} to upload ({} already present) [{:.2}s]", files.len(), todo.len(), files.len() - todo.len(), t.elapsed().as_secs_f64());
    let s3 = &s3;
    let (bucket, prefix, cc) = (&a.bucket, &prefix, &a.cache_control);
    let res: Vec<Result<()>> = stream::iter(todo).map(|(p, r)| async move {
        let body = tokio::fs::read(p).await.with_context(|| format!("read {}", p.display()))?;
        s3.put(bucket, &format!("{prefix}/{r}"), body, content_type(r), cc).await
    }).buffer_unordered(a.concurrency).collect().await;
    let errs: Vec<_> = res.into_iter().filter_map(|r| r.err()).collect();
    if !errs.is_empty() {
        bail!("{} uploads failed, first: {}", errs.len(), errs[0]);
    }
    if let Some(m) = &a.marker {
        s3.put(&a.bucket, m.trim_matches('/'), b"ok\n".to_vec(), "application/octet-stream", "").await?;
    }
    eprintln!("cdn: done [{:.2}s]", t.elapsed().as_secs_f64());
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn encodes_like_sigv4() {
        assert_eq!(super::enc("a b/c+d", true), "a%20b/c%2Bd");
        assert_eq!(super::enc("a/b", false), "a%2Fb");
    }
}
