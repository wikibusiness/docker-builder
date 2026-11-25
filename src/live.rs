//! Live staging pulls. While an image is pushed, its manifest and the layers being uploaded are also sent to a staging
//! registry (`IMGBUILD_STREAM_URL`), which serves them to pulls that started before the push ended. Everything here is
//! best effort: the registry the image is pushed to stays the source of truth and a failure never fails a build.
use anyhow::{Result, bail};
use bytes::Bytes;
use futures::stream;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::path::Path;
use tokio::io::AsyncReadExt;

#[derive(Clone)]
pub struct Live {
    urls: Vec<String>,
    c: reqwest::Client,
    auth: Option<(String, String)>,
}

/// Every target gets the call; it succeeds if one took it, and the others' failures are printed.
async fn all<F: std::future::Future<Output = Result<()>>>(calls: impl IntoIterator<Item = F>) -> Result<()> {
    let results = futures::future::join_all(calls).await;
    let ok = results.iter().any(|r| r.is_ok());
    for e in results.into_iter().filter_map(Result::err) {
        eprintln!("live: {e:#}");
    }
    if ok { Ok(()) } else { bail!("no staging registry took it") }
}

impl Live {
    pub fn from_env(dest_host: &str) -> Option<Live> {
        let urls: Vec<String> = std::env::var("IMGBUILD_STREAM_URL")
            .ok()?
            .split(',')
            .map(|u| u.trim().trim_end_matches('/').to_string())
            .filter(|u| !u.is_empty())
            .collect();
        (!urls.is_empty()).then(|| Live { urls, c: reqwest::Client::new(), auth: crate::registry::creds_for(dest_host) })
    }

    /// Every replica that serves pulls keeps its own copies, so each one gets them. A name that resolves to several
    /// addresses (a headless Service) is one URL per address.
    async fn targets(&self) -> Vec<String> {
        let mut out = vec![];
        for url in &self.urls {
            let Ok(parsed) = reqwest::Url::parse(url) else { continue };
            let (Some(host), Some(port)) = (parsed.host_str(), parsed.port_or_known_default()) else { continue };
            match tokio::net::lookup_host((host, port)).await {
                Ok(addrs) => {
                    for addr in addrs {
                        let mut u = parsed.clone();
                        if u.set_ip_host(addr.ip()).is_ok() {
                            let one = u.as_str().trim_end_matches('/').to_string();
                            if !out.contains(&one) {
                                out.push(one);
                            }
                        }
                    }
                }
                Err(_) => out.push(url.clone()),
            }
        }
        out
    }

    fn put(&self, base: &str, path: &str) -> reqwest::RequestBuilder {
        self.with_auth(self.c.put(format!("{base}/_stream/{path}")))
    }

    fn with_auth(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.auth {
            Some((u, p)) => rb.basic_auth(u, Some(p)),
            None => rb,
        }
    }

    pub async fn manifest(&self, repo: &str, reference: &str, media_type: &str, body: Bytes) -> Result<()> {
        all(self.targets().await.into_iter().map(|base| {
            let body = body.clone();
            async move {
                let r = self.put(&base, &format!("manifest/{repo}/{reference}")).header("Content-Type", media_type).body(body).send().await?;
                if !r.status().is_success() {
                    bail!("announce {repo}:{reference} to {base}: {}", r.status());
                }
                Ok(())
            }
        }))
        .await
    }

    pub async fn blob_bytes(&self, repo: &str, digest: &str, body: Bytes) -> Result<()> {
        all(self.targets().await.into_iter().map(|base| {
            let body = body.clone();
            async move {
                let r = self.put(&base, &format!("blob/{repo}/{digest}")).body(body).send().await?;
                if !r.status().is_success() {
                    bail!("copy {digest} to {base}: {}", r.status());
                }
                Ok(())
            }
        }))
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn prefetch(&self, repo: &str, tag: &str, platform: &str, digest: &str, diff_id: &str, size: u64, media_type: &str) -> Result<()> {
        let mut platform = platform.split('/');
        let config = Bytes::from(serde_json::to_vec(&json!({
            "os": platform.next(), "architecture": platform.next(),
            "rootfs": {"type": "layers", "diff_ids": [diff_id]},
        }))?);
        let config_digest = format!("sha256:{}", hex::encode(Sha256::digest(&config)));
        let kind = "application/vnd.oci.image.manifest.v1+json";
        let manifest = Bytes::from(serde_json::to_vec(&json!({
            "schemaVersion": 2, "mediaType": kind,
            "config": {"mediaType": "application/vnd.oci.image.config.v1+json", "digest": config_digest, "size": config.len()},
            "layers": [{"mediaType": media_type, "digest": digest, "size": size}],
        }))?);
        self.blob_bytes(repo, &config_digest, config).await?;
        self.manifest(repo, tag, kind, manifest).await
    }

    /// Streams a file as it is read, so a multi-GB layer is never held in memory.
    pub async fn blob_file(&self, repo: &str, digest: &str, path: &Path) -> Result<()> {
        all(self.targets().await.into_iter().map(|base| self.blob_file_to(base, repo, digest, path)))
            .await
    }

    async fn blob_file_to(&self, base: String, repo: &str, digest: &str, path: &Path) -> Result<()> {
        const CHUNK: usize = 1 << 20;
        let size = tokio::fs::metadata(path).await?.len();
        let file = tokio::fs::File::open(path).await?;
        let body = reqwest::Body::wrap_stream(stream::unfold(file, |mut f| async move {
            let mut buf = vec![0u8; CHUNK];
            match f.read(&mut buf).await {
                Ok(0) => None,
                Ok(n) => {
                    buf.truncate(n);
                    Some((Ok::<_, std::io::Error>(Bytes::from(buf)), f))
                }
                Err(e) => Some((Err(e), f)),
            }
        }));
        let r = self.put(&base, &format!("blob/{repo}/{digest}")).header("Content-Length", size).body(body).send().await?;
        if !r.status().is_success() {
            bail!("copy {digest} to {base}: {}", r.status());
        }
        Ok(())
    }
}

#[cfg(test)]
#[tokio::test]
async fn every_address_of_every_url_is_a_target_once() {
    let live = Live {
        urls: vec!["http://127.0.0.1:5000".into(), "http://127.0.0.1:5000".into(), "http://127.0.0.2:5000/".into()],
        c: reqwest::Client::new(),
        auth: None,
    };
    assert_eq!(live.targets().await, ["http://127.0.0.1:5000", "http://127.0.0.2:5000"]);
}
